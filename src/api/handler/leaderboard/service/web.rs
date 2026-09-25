use serde::Deserialize;

use crate::api::error::ApiError;
use crate::api::extract::{ApiAudience, prepare_audience_user_id_mode, resolve_region_engine};
use crate::api::handler::web::{build_overview, build_world_bloom_overview, cached_overview_bytes};
use crate::api::json::{EncodedJson, Json};
use crate::api::state::AppState;
use crate::model::api::{
    LeaderboardOverviewSchema, RecordedRankData, WebRankDetailResponseSchema, WebRankingItemSchema,
    WebSubjectSchema, WebUserDetailResponseSchema,
};

use super::snapshot::{
    SnapshotBuildRequest, build_rank_snapshots_response, ensure_current_is_user, resolve_rank_cut,
    resolve_user_rank,
};
use super::trace::{SubjectTraceQuery, build_subject_trace_response};
use super::util::{interval_seconds, meta, positive_timestamp, user_id_of_rank_data};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewQuery {
    interval: Option<i64>,
    at: Option<i64>,
}

/// A slice of the overview served as its own resource (`.../top100`,
/// `.../borders`, `.../growth`). Parts are cut out of the cached overview
/// of the same version, so every part of one `version` comes from one
/// computation, and the overview's builders (and any fix to them) are the
/// only source of the data.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OverviewPart {
    Top100,
    Borders,
    Growth,
}

/// `(field, value when the overview omitted it)`; `None` = omit too.
type PartField = (&'static str, Option<&'static str>);

impl OverviewPart {
    fn name(self) -> &'static str {
        match self {
            Self::Top100 => "top100",
            Self::Borders => "borders",
            Self::Growth => "growth",
        }
    }

    /// The overview fields each part carries, in output order. Lists the
    /// overview skips when empty come back as `[]`.
    fn fields(self) -> &'static [PartField] {
        match self {
            Self::Top100 => &[
                ("meta", None),
                ("topRankings", Some("[]")),
                ("status", None),
            ],
            Self::Borders => &[
                ("meta", None),
                ("borderLines", Some("[]")),
                ("status", None),
            ],
            Self::Growth => &[
                ("meta", None),
                ("topPlayerGrowths", Some("[]")),
                ("topRankGrowths", Some("[]")),
                ("borderGrowths", Some("[]")),
                ("intervalSeconds", None),
                ("windowStart", None),
                ("windowEnd", None),
            ],
        }
    }
}

/// Copies the part's fields out of an overview body verbatim (no tree is
/// built and nothing is re-encoded).
fn project_overview_part(overview: &[u8], part: OverviewPart) -> Result<String, ApiError> {
    let fields = part.fields();
    let mut raw: Vec<Option<std::borrow::Cow<'_, str>>> = vec![None; fields.len()];
    for entry in sonic_rs::to_object_iter(overview) {
        let (key, value) = entry.map_err(|err| {
            tracing::warn!(%err, "overview body is not a JSON object");
            ApiError::ServiceUnavailable("overview decode failed".into())
        })?;
        if let Some(index) = fields.iter().position(|(name, _)| key == *name) {
            raw[index] = Some(value.as_raw_cow());
        }
    }
    let mut out = String::with_capacity(overview.len() / 2 + 64);
    out.push('{');
    for ((name, fallback), value) in fields.iter().zip(&raw) {
        let Some(value) = value.as_deref().or(*fallback) else {
            continue;
        };
        if out.len() > 1 {
            out.push(',');
        }
        out.push('"');
        out.push_str(name);
        out.push_str("\":");
        out.push_str(value);
    }
    out.push('}');
    Ok(out)
}

/// Already-encoded JSON that serializes as itself.
struct RawJsonText(String);

impl serde::Serialize for RawJsonText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value: sonic_rs::LazyValue<'_> =
            sonic_rs::from_str(&self.0).map_err(serde::ser::Error::custom)?;
        value.serialize(serializer)
    }
}

pub(crate) async fn web_overview_part_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    part: OverviewPart,
    query: OverviewQuery,
    prefer_gzip: bool,
) -> Result<EncodedJson, ApiError> {
    let interval = interval_seconds(query.interval);
    let at = positive_timestamp(query.at);
    let suffix = format!(
        "{}:part={}",
        overview_suffix(WEB_OVERVIEW_PREFIX, character_id, interval, at, None),
        part.name()
    );
    let fetch = async {
        // Boxed: the overview's own cache + build future nested inline makes
        // this handler's future (and debug-build stack frames) very large.
        let overview = Box::pin(web_overview_for_scope(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            query,
            WEB_OVERVIEW_PREFIX,
            false,
        ))
        .await?
        .into_identity_bytes()
        .ok_or_else(|| ApiError::ServiceUnavailable("overview encoding mismatch".into()))?;
        project_overview_part(&overview, part).map(RawJsonText)
    };
    cached_overview_bytes(
        &state,
        &server,
        event_id,
        suffix,
        at.is_some(),
        prefer_gzip,
        fetch,
    )
    .await
}

pub(crate) const WEB_OVERVIEW_PREFIX: &str = "web:v2";

fn overview_suffix(
    cache_prefix: &str,
    character_id: Option<i64>,
    interval: i64,
    at: Option<i64>,
    cut: Option<i64>,
) -> String {
    match character_id {
        Some(character_id) => format!(
            "{cache_prefix}:wb:{character_id}:overview:interval={interval}:at={at:?}:cut={cut:?}"
        ),
        None => {
            format!("{cache_prefix}:total:overview:interval={interval}:at={at:?}:cut={cut:?}")
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDetailQuery {
    interval: Option<i64>,
    at: Option<i64>,
    include_trace: Option<bool>,
    include_player_trace: Option<bool>,
    include_profile: Option<bool>,
    cursor: Option<i64>,
    limit: Option<u64>,
    /// `details/user/{id}` only: `unique` (default) or `uid` to look the
    /// player up by raw upstream UID.
    id_type: Option<String>,
    /// `check-room` only: the raw upstream UID to look up.
    user_id: Option<String>,
}

impl WebDetailQuery {
    /// Whether `details/user/{user_id}` resolves `user_id` as a raw upstream
    /// UID — such a response carries that UID and must stay private.
    pub(crate) fn looks_up_raw_uid(&self, user_id: &str) -> Result<bool, ApiError> {
        match self.id_type.as_deref().map(str::trim) {
            // A bare numeric id can only be a game UID (unique_ids are hex
            // digests), so it is treated as an explicit raw lookup.
            None | Some("") => Ok(looks_like_raw_uid(user_id)),
            Some("unique") => Ok(false),
            Some("uid") => Ok(true),
            Some(other) => Err(ApiError::BadRequest(format!(
                "idType must be unique or uid, got {other}"
            ))),
        }
    }
}

const MAX_RAW_UID_LEN: usize = 30;

fn looks_like_raw_uid(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= MAX_RAW_UID_LEN
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u128>().is_ok_and(|id| id > 0)
}

fn validate_raw_uid(raw: &str) -> Result<&str, ApiError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_RAW_UID_LEN || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::BadRequest(
            "userId must be a numeric upstream uid".into(),
        ));
    }
    Ok(raw)
}

pub(crate) async fn web_overview_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: OverviewQuery,
    cache_prefix: &str,
    prefer_gzip: bool,
) -> Result<EncodedJson, ApiError> {
    let interval = interval_seconds(query.interval);
    let at = positive_timestamp(query.at);
    let end_time = at.unwrap_or_else(|| chrono::Utc::now().timestamp());
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let cut = Box::pin(resolve_rank_cut(
        &state,
        &server,
        &engine,
        event_id,
        character_id,
        at,
    ))
    .await?;
    let suffix = overview_suffix(cache_prefix, character_id, interval, at, cut.as_of_time_id);
    let cache_server = server.clone();
    let fetch = async {
        let mode =
            prepare_audience_user_id_mode(&state, &engine, region, event_id, ApiAudience::Web)
                .await?;
        let overview = match character_id {
            Some(character_id) => {
                build_world_bloom_overview(&engine, event_id, character_id, mode, interval, cut)
                    .await?
            }
            None => build_overview(&engine, event_id, mode, interval, cut).await?,
        };
        Ok(LeaderboardOverviewSchema {
            meta: meta(&server, event_id, character_id, end_time),
            overview,
            window_start: end_time - interval,
            window_end: end_time,
        })
    };
    cached_overview_bytes(
        &state,
        &cache_server,
        event_id,
        suffix,
        at.is_some(),
        prefer_gzip,
        fetch,
    )
    .await
}

pub(crate) async fn web_rank_detail_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    rank: i64,
    query: WebDetailQuery,
) -> Result<Json<WebRankDetailResponseSchema>, ApiError> {
    if rank <= 0 {
        return Err(ApiError::BadRequest("rank must be positive".into()));
    }
    let interval = interval_seconds(query.interval);
    let at = positive_timestamp(query.at);
    let snapshot = build_rank_snapshots_response(
        state.clone(),
        server.clone(),
        event_id,
        character_id,
        SnapshotBuildRequest {
            ranks: vec![rank],
            include_adjacent: true,
            include_metrics: true,
            interval,
            at,
            cache_prefix: "web:v2",
            audience: ApiAudience::Web,
            cut: None,
        },
    )
    .await?;
    let item = snapshot
        .items
        .into_iter()
        .find(|item| item.rank == rank)
        .ok_or(ApiError::NotFound)?;
    let mut rank_trace = Vec::new();
    if query.include_trace.unwrap_or(false) {
        rank_trace = build_subject_trace_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            rank.to_string(),
            detail_trace_query(&query, "rank"),
            "web:v2",
            ApiAudience::Web,
        )
        .await?
        .rank_data;
    }
    let mut player_trace = Vec::new();
    if query.include_player_trace.unwrap_or(false)
        && let Some(current) = item.current.as_ref()
        && let Some(user_id) = user_id_of_rank_data(&current.rank_data)
    {
        player_trace = build_subject_trace_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            user_id,
            detail_trace_query(&query, "user"),
            "web:v2",
            ApiAudience::Web,
        )
        .await?
        .rank_data;
    }
    Ok(Json(WebRankDetailResponseSchema {
        meta: snapshot.meta,
        current: item.current,
        previous: item.previous,
        next: item.next,
        metrics: item.metrics,
        rank_trace,
        player_trace,
        interval_seconds: snapshot.interval_seconds,
        window_start: snapshot.window_start,
        window_end: snapshot.window_end,
    }))
}

/// `details/user/{id}`: `id` is a public `unique_id` unless
/// `idType=uid`, in which case it is a raw upstream UID and the response
/// reveals that one player's raw UID (see `web_user_detail_by_raw_uid`).
pub(crate) async fn web_user_detail_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    user_id: String,
    query: WebDetailQuery,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    if query.looks_up_raw_uid(&user_id)? {
        web_user_detail_by_raw_uid(state, server, event_id, character_id, user_id, query).await
    } else {
        web_user_detail_by_unique_id(state, server, event_id, character_id, user_id, query).await
    }
}

/// `check-room?userId=<raw uid>`: the web counterpart of the cloud
/// check-room, keyed by the exact upstream UID the caller typed.
pub(crate) async fn web_check_room_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: WebDetailQuery,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    let raw = query
        .user_id
        .clone()
        .ok_or_else(|| ApiError::BadRequest("userId is required".into()))?;
    web_user_detail_by_raw_uid(state, server, event_id, character_id, raw, query).await
}

/// Map the raw UID to its `unique_id` (the anonymizer is deterministic, so
/// no lookup is needed), serve the ordinary cached anonymized user detail
/// for it, then swap the raw UID back in for the subject only. Neighbours
/// and the cache never see the raw value; an untracked UID is a plain 404.
async fn web_user_detail_by_raw_uid(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    raw_user_id: String,
    query: WebDetailQuery,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    let raw = validate_raw_uid(&raw_user_id)?.to_owned();
    let (region, _) = resolve_region_engine(&state, &server)?;
    if !state.anonymizer().is_enabled() {
        return Err(ApiError::BadRequest(
            "web API requires privacy.uid_anonymization.enabled".into(),
        ));
    }
    let unique_id = state.anonymizer().public_user_id(region, event_id, &raw);
    let Json(mut detail) = web_user_detail_by_unique_id(
        state,
        server,
        event_id,
        character_id,
        unique_id.clone(),
        query,
    )
    .await?;
    reveal_subject(&mut detail, &unique_id, &raw);
    detail.subject = Some(WebSubjectSchema {
        user_id: raw,
        unique_id,
    });
    Ok(Json(detail))
}

fn reveal_subject(detail: &mut WebUserDetailResponseSchema, unique_id: &str, raw: &str) {
    if let Some(current) = detail.current.as_mut() {
        reveal_item(current, unique_id, raw);
    }
    for row in &mut detail.player_trace {
        reveal_rank_data(row, unique_id, raw);
    }
    if let Some(profile) = detail.profile.as_mut()
        && profile.user_id == unique_id
    {
        profile.user_id = raw.to_owned();
    }
}

fn reveal_item(item: &mut WebRankingItemSchema, unique_id: &str, raw: &str) {
    reveal_rank_data(&mut item.rank_data, unique_id, raw);
    if let Some(user) = item.user_data.as_mut()
        && user.user_id == unique_id
    {
        user.user_id = raw.to_owned();
    }
}

fn reveal_rank_data(data: &mut RecordedRankData, unique_id: &str, raw: &str) {
    let user_id = match data {
        RecordedRankData::Normal(row) => &mut row.user_id,
        RecordedRankData::WorldBloom(row) => &mut row.user_id,
    };
    if user_id == unique_id {
        *user_id = raw.to_owned();
    }
}

async fn web_user_detail_by_unique_id(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    user_id: String,
    query: WebDetailQuery,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    let at = positive_timestamp(query.at);
    let (rank, cut) = Box::pin(resolve_user_rank(
        &state,
        &server,
        event_id,
        character_id,
        &user_id,
        at,
        ApiAudience::Web,
    ))
    .await?;
    let snapshot = Box::pin(build_rank_snapshots_response(
        state.clone(),
        server.clone(),
        event_id,
        character_id,
        SnapshotBuildRequest {
            ranks: vec![rank],
            include_adjacent: true,
            include_metrics: false,
            interval: interval_seconds(query.interval),
            at,
            cache_prefix: "web:v2",
            audience: ApiAudience::Web,
            cut: Some(cut),
        },
    ))
    .await?;
    ensure_current_is_user(&snapshot, rank, &user_id)?;
    let item = snapshot
        .items
        .into_iter()
        .find(|item| item.rank == rank)
        .ok_or(ApiError::NotFound)?;
    let profile = if query.include_profile.unwrap_or(false) {
        build_subject_trace_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            user_id.clone(),
            SubjectTraceQuery {
                subject_type: Some("user".to_owned()),
                include_current: Some(true),
                start_time: None,
                end_time: None,
                cursor: None,
                limit: Some(1),
            },
            "web:v2",
            ApiAudience::Web,
        )
        .await?
        .user_data
    } else {
        None
    };
    let player_trace = if query.include_trace.unwrap_or(false) {
        build_subject_trace_response(
            state,
            server,
            event_id,
            character_id,
            user_id,
            detail_trace_query(&query, "user"),
            "web:v2",
            ApiAudience::Web,
        )
        .await?
        .rank_data
    } else {
        Vec::new()
    };
    Ok(Json(WebUserDetailResponseSchema {
        meta: snapshot.meta,
        subject: None,
        current: item.current,
        previous: item.previous,
        next: item.next,
        player_trace,
        profile,
    }))
}

fn detail_trace_query(query: &WebDetailQuery, subject_type: &str) -> SubjectTraceQuery {
    SubjectTraceQuery {
        subject_type: Some(subject_type.to_owned()),
        include_current: Some(true),
        start_time: None,
        end_time: None,
        cursor: query.cursor,
        limit: query.limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::realtime::RealtimeHub;
    use crate::api::state::AppState;
    use crate::api::ws_ticket::WsTicketStore;
    use crate::config::ApiQueryConfig;
    use crate::db::query::web::tests::{
        seed_normal_event_with_history, seed_world_bloom_event_with_history, sqlite_engine,
    };
    use crate::db::schema::create_event_tables;
    use crate::model::enums::SekaiServerRegion;
    use crate::privacy::UidAnonymizer;
    use axum::response::IntoResponse;
    use std::collections::HashMap;
    use std::sync::Arc;

    const NORMAL_EVENT: i64 = 811;
    const WORLD_BLOOM_EVENT: i64 = 812;

    async fn test_state() -> AppState {
        test_state_with_anonymizer(UidAnonymizer::enabled("salt")).await
    }

    async fn test_state_with_anonymizer(anonymizer: UidAnonymizer) -> AppState {
        let engine = sqlite_engine().await;
        create_event_tables(&engine, SekaiServerRegion::Jp, NORMAL_EVENT, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, NORMAL_EVENT).await;
        create_event_tables(&engine, SekaiServerRegion::Jp, WORLD_BLOOM_EVENT, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_history(&engine, WORLD_BLOOM_EVENT).await;
        AppState::new(
            HashMap::from([(SekaiServerRegion::Jp, Arc::new(engine))]),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), [SekaiServerRegion::Jp]),
            anonymizer,
            None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
    }

    fn unique(state: &AppState, event_id: i64, raw: &str) -> String {
        state
            .anonymizer()
            .public_user_id(SekaiServerRegion::Jp, event_id, raw)
    }

    fn detail_query() -> WebDetailQuery {
        WebDetailQuery {
            interval: Some(60),
            at: Some(1_710_000_060),
            include_trace: Some(true),
            include_player_trace: Some(true),
            include_profile: Some(true),
            cursor: None,
            limit: Some(10),
            ..WebDetailQuery::default()
        }
    }

    #[test]
    fn web_overviews_cover_live_replay_and_world_bloom() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let state = test_state().await;
                        let replay = web_overview_for_scope(
                            state.clone(),
                            "jp".into(),
                            NORMAL_EVENT,
                            None,
                            OverviewQuery {
                                interval: Some(60),
                                at: Some(1_710_000_060),
                            },
                            "web:v2",
                            false,
                        )
                        .await
                        .unwrap();
                        assert!(replay.into_response().status().is_success());

                        let live = web_overview_for_scope(
                            state.clone(),
                            "jp".into(),
                            NORMAL_EVENT,
                            None,
                            OverviewQuery {
                                interval: Some(60),
                                at: None,
                            },
                            "web:v2",
                            true,
                        )
                        .await
                        .unwrap();
                        assert!(live.into_response().status().is_success());

                        let world = web_overview_for_scope(
                            state,
                            "jp".into(),
                            WORLD_BLOOM_EVENT,
                            Some(17),
                            OverviewQuery {
                                interval: Some(60),
                                at: Some(1_710_000_060),
                            },
                            "web:v2",
                            false,
                        )
                        .await
                        .unwrap();
                        assert!(world.into_response().status().is_success());
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[tokio::test]
    async fn web_rank_details_include_metrics_and_traces() {
        let state = test_state().await;
        let detail = web_rank_detail_for_scope(
            state.clone(),
            "jp".into(),
            NORMAL_EVENT,
            None,
            2,
            detail_query(),
        )
        .await
        .unwrap()
        .0;
        assert!(detail.current.is_some());
        assert!(detail.previous.is_some());
        assert!(detail.next.is_some());
        assert!(detail.metrics.is_some());
        assert_eq!(detail.rank_trace.len(), 2);
        assert_eq!(detail.player_trace.len(), 2);

        let world = web_rank_detail_for_scope(
            state.clone(),
            "jp".into(),
            WORLD_BLOOM_EVENT,
            Some(17),
            1,
            detail_query(),
        )
        .await
        .unwrap()
        .0;
        assert!(world.current.is_some());
        assert!(!world.rank_trace.is_empty());

        let error =
            web_rank_detail_for_scope(state, "jp".into(), NORMAL_EVENT, None, 0, detail_query())
                .await
                .err()
                .expect("non-positive rank must fail");
        assert!(matches!(error, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn web_user_details_include_profile_and_optional_trace() {
        let state = test_state().await;
        let detail = web_user_detail_for_scope(
            state.clone(),
            "jp".into(),
            NORMAL_EVENT,
            None,
            unique(&state, NORMAL_EVENT, "100"),
            detail_query(),
        )
        .await
        .unwrap()
        .0;
        assert!(detail.current.is_some());
        assert!(detail.subject.is_none());
        assert_eq!(detail.profile.unwrap().name, "Alpha");
        assert_eq!(detail.player_trace.len(), 2);

        let mut without_trace = detail_query();
        without_trace.include_trace = Some(false);
        without_trace.include_profile = Some(false);
        let world_unique = unique(&state, WORLD_BLOOM_EVENT, "100");
        let world = web_user_detail_for_scope(
            state,
            "jp".into(),
            WORLD_BLOOM_EVENT,
            Some(17),
            world_unique,
            without_trace,
        )
        .await
        .unwrap()
        .0;
        assert!(world.current.is_some());
        assert!(world.profile.is_none());
        assert!(world.player_trace.is_empty());
    }

    fn user_id_of(item: &WebRankingItemSchema) -> String {
        user_id_of_rank_data(&item.rank_data).unwrap()
    }

    #[tokio::test]
    async fn raw_uid_lookups_reveal_only_the_subject() {
        let state = test_state().await;
        let expected_unique = unique(&state, NORMAL_EVENT, "100");
        let mut query = detail_query();
        query.user_id = Some("100".into());
        let detail =
            web_check_room_for_scope(state.clone(), "jp".into(), NORMAL_EVENT, None, query)
                .await
                .unwrap()
                .0;
        let subject = detail.subject.clone().unwrap();
        assert_eq!(subject.user_id, "100");
        assert_eq!(subject.unique_id, expected_unique);
        let current = detail.current.as_ref().unwrap();
        assert_eq!(user_id_of(current), "100");
        assert_eq!(current.user_data.as_ref().unwrap().user_id, "100");
        assert_eq!(detail.profile.as_ref().unwrap().user_id, "100");
        assert!(
            detail
                .player_trace
                .iter()
                .all(|row| { user_id_of_rank_data(row).as_deref() == Some("100") })
        );
        // Neighbours stay anonymized: never the raw form, never the subject.
        for neighbour in [detail.previous.as_ref(), detail.next.as_ref()]
            .into_iter()
            .flatten()
        {
            let id = user_id_of(neighbour);
            assert_ne!(id, "100");
            assert!(!id.bytes().all(|b| b.is_ascii_digit()), "{id}");
        }

        // `details/user/{raw}?idType=uid` is the same lookup.
        let mut by_type = detail_query();
        by_type.id_type = Some("uid".into());
        let via_detail = web_user_detail_for_scope(
            state.clone(),
            "jp".into(),
            NORMAL_EVENT,
            None,
            "100".into(),
            by_type,
        )
        .await
        .unwrap()
        .0;
        assert_eq!(via_detail.subject, detail.subject);

        // World Bloom scope resolves through the same users table.
        let mut world = detail_query();
        world.user_id = Some("100".into());
        let world_detail = web_check_room_for_scope(
            state.clone(),
            "jp".into(),
            WORLD_BLOOM_EVENT,
            Some(17),
            world,
        )
        .await
        .unwrap()
        .0;
        assert_eq!(world_detail.subject.unwrap().user_id, "100");

        // Validation and misses.
        let mut missing = detail_query();
        missing.user_id = Some("424242".into());
        assert!(matches!(
            web_check_room_for_scope(state.clone(), "jp".into(), NORMAL_EVENT, None, missing).await,
            Err(ApiError::NotFound)
        ));
        let mut bad = detail_query();
        bad.user_id = Some("not-a-uid".into());
        assert!(matches!(
            web_check_room_for_scope(state.clone(), "jp".into(), NORMAL_EVENT, None, bad).await,
            Err(ApiError::BadRequest(_))
        ));
        assert!(matches!(
            web_check_room_for_scope(
                state.clone(),
                "jp".into(),
                NORMAL_EVENT,
                None,
                detail_query()
            )
            .await,
            Err(ApiError::BadRequest(_))
        ));
        let mut bogus = detail_query();
        bogus.id_type = Some("bogus".into());
        assert!(matches!(
            web_user_detail_for_scope(state, "jp".into(), NORMAL_EVENT, None, "100".into(), bogus)
                .await,
            Err(ApiError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn bare_numeric_ids_are_game_uids_and_reveal_only_that_player() {
        let state = test_state_with_anonymizer(UidAnonymizer::enabled("test-salt")).await;
        for (event, chapter) in [(NORMAL_EVENT, None), (WORLD_BLOOM_EVENT, Some(17))] {
            let public_id = state
                .anonymizer()
                .public_user_id(SekaiServerRegion::Jp, event, "100");
            let by_uid = web_user_detail_for_scope(
                state.clone(),
                "jp".into(),
                event,
                chapter,
                "100".into(),
                detail_query(),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(by_uid.subject.as_ref().unwrap().unique_id, public_id);
            assert_eq!(user_id_of(by_uid.current.as_ref().unwrap()), "100");
            assert!(!by_uid.player_trace.is_empty());

            let by_unique = web_user_detail_for_scope(
                state.clone(),
                "jp".into(),
                event,
                chapter,
                public_id.clone(),
                detail_query(),
            )
            .await
            .unwrap()
            .0;
            assert!(by_unique.subject.is_none());
            assert_eq!(user_id_of(by_unique.current.as_ref().unwrap()), public_id);
        }
    }

    #[test]
    fn detail_trace_query_forwards_cursor_and_limit() {
        let query = WebDetailQuery {
            interval: None,
            at: None,
            include_trace: Some(true),
            include_player_trace: None,
            include_profile: None,
            cursor: Some(1_786_726_540),
            limit: Some(5_000),
            ..WebDetailQuery::default()
        };

        let trace_query = detail_trace_query(&query, "user");

        assert_eq!(trace_query.subject_type.as_deref(), Some("user"));
        assert_eq!(trace_query.cursor, Some(1_786_726_540));
        assert_eq!(trace_query.limit, Some(5_000));
    }

    #[test]
    fn overview_parts_project_fields_verbatim() {
        let overview = br#"{"meta":{"server":"jp","eventId":1},"topRankings":[{"rankData":{"score":1.50}}],"intervalSeconds":60,"windowStart":0,"windowEnd":60}"#;
        assert_eq!(
            project_overview_part(overview, OverviewPart::Top100).unwrap(),
            r#"{"meta":{"server":"jp","eventId":1},"topRankings":[{"rankData":{"score":1.50}}]}"#
        );
        // Lists the overview skipped come back empty; a missing status
        // stays missing.
        assert_eq!(
            project_overview_part(overview, OverviewPart::Borders).unwrap(),
            r#"{"meta":{"server":"jp","eventId":1},"borderLines":[]}"#
        );
        assert_eq!(
            project_overview_part(overview, OverviewPart::Growth).unwrap(),
            r#"{"meta":{"server":"jp","eventId":1},"topPlayerGrowths":[],"topRankGrowths":[],"borderGrowths":[],"intervalSeconds":60,"windowStart":0,"windowEnd":60}"#
        );
        assert!(project_overview_part(b"[1]", OverviewPart::Top100).is_err());
        assert!(project_overview_part(b"{\"meta\":", OverviewPart::Top100).is_err());
        assert_eq!(
            sonic_rs::to_string(&RawJsonText(r#"{"a":[1,2.50]}"#.into())).unwrap(),
            r#"{"a":[1,2.50]}"#
        );
    }

    #[test]
    fn overview_parts_carry_exactly_the_overview_fields() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let state = test_state().await;
                        let query = OverviewQuery {
                            interval: Some(60),
                            at: Some(1_710_000_060),
                        };
                        for (event_id, character_id) in
                            [(NORMAL_EVENT, None), (WORLD_BLOOM_EVENT, Some(17))]
                        {
                            let overview: serde_json::Value = serde_json::from_slice(
                                &web_overview_for_scope(
                                    state.clone(),
                                    "jp".into(),
                                    event_id,
                                    character_id,
                                    query,
                                    WEB_OVERVIEW_PREFIX,
                                    false,
                                )
                                .await
                                .unwrap()
                                .into_identity_bytes()
                                .unwrap(),
                            )
                            .unwrap();
                            assert!(!overview["topRankings"].as_array().unwrap().is_empty());
                            let mut covered = std::collections::BTreeSet::new();
                            for part in [
                                OverviewPart::Top100,
                                OverviewPart::Borders,
                                OverviewPart::Growth,
                            ] {
                                let body = web_overview_part_for_scope(
                                    state.clone(),
                                    "jp".into(),
                                    event_id,
                                    character_id,
                                    part,
                                    query,
                                    false,
                                )
                                .await
                                .unwrap()
                                .into_identity_bytes()
                                .unwrap();
                                let value: serde_json::Value =
                                    serde_json::from_slice(&body).unwrap();
                                let object = value.as_object().unwrap();
                                for (name, _) in part.fields() {
                                    let expected = overview
                                        .get(*name)
                                        .cloned()
                                        .unwrap_or_else(|| serde_json::json!([]));
                                    if *name == "status" && overview.get("status").is_none() {
                                        assert!(!object.contains_key("status"));
                                        continue;
                                    }
                                    assert_eq!(
                                        object.get(*name),
                                        Some(&expected),
                                        "{part:?} {name}"
                                    );
                                    covered.insert(*name);
                                }
                                assert!(
                                    object
                                        .keys()
                                        .all(|key| part.fields().iter().any(|(n, _)| n == key))
                                );
                            }
                            // Every overview field lives in some part.
                            for key in overview.as_object().unwrap().keys() {
                                assert!(covered.contains(key.as_str()), "{key} is in no part");
                            }
                        }
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
