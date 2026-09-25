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
    SnapshotBuildRequest, build_rank_snapshots_response, resolve_rank_cut, resolve_user_rank,
};
use super::trace::{SubjectTraceQuery, build_subject_trace_response};
use super::util::{interval_seconds, meta, positive_timestamp, user_id_of_rank_data};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewQuery {
    interval: Option<i64>,
    at: Option<i64>,
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
    let cut_key = cut.as_of_time_id;
    let suffix = match character_id {
        Some(character_id) => format!(
            "{cache_prefix}:wb:{character_id}:overview:interval={interval}:at={at:?}:cut={cut_key:?}"
        ),
        None => {
            format!("{cache_prefix}:total:overview:interval={interval}:at={at:?}:cut={cut_key:?}")
        }
    };
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
    let by_raw_uid = match query.id_type.as_deref().map(str::trim) {
        // A bare numeric id can only be a game UID (unique_ids are hex
        // digests), so it is treated as an explicit raw lookup.
        None | Some("") => looks_like_raw_uid(&user_id),
        Some("unique") => false,
        Some("uid") => true,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "idType must be unique or uid, got {other}"
            )));
        }
    };
    if by_raw_uid {
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
}
