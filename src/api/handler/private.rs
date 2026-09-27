//! Login-protected raw UID lookup endpoints for Toolbox-bound accounts.
//!
//! Public ranking/user endpoints switch to anonymized unique IDs when privacy
//! mode is enabled. These private endpoints keep raw UID lookup available only
//! when the trusted Oathkeeper/WebSocket subject owns the requested Toolbox game
//! account binding.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::api::access_log::ProxyTrust;
use crate::api::error::ApiError;
use crate::api::extract::{prepare_private_user_id_mode, resolve_region_engine};
use crate::api::json::Json;
use crate::api::private_lookup::PrivateLookupError;
use crate::api::state::AppState;
use crate::api::ws_ticket::{resolve_trusted_subject, unauthorized};
use crate::db::query::ranking::{
    fetch_all_rankings, fetch_latest_ranking, fetch_latest_ranking_by_rank,
};
use crate::db::query::user::get_user_data;
use crate::db::query::web::{WebTraceFilter, search_user_trace, search_world_bloom_user_trace};
use crate::db::query::world_bloom::{
    fetch_all_world_bloom_rankings, fetch_latest_world_bloom_ranking,
    fetch_latest_world_bloom_ranking_by_rank,
};
use crate::model::api::{
    LeaderboardMetaSchema, RecordedRankData, UserAllRankingDataQueryResponseSchema,
    UserLatestRankingQueryResponseSchema, WebRankingItemSchema, WebUserDetailResponseSchema,
};
use crate::model::enums::SekaiServerRegion;
use crate::model::trace_columns::{TraceFormat, TracePayload};

/// Same bound as the public web detail's trace page.
const MAX_TRACE_LIMIT: u64 = 10_000;

#[derive(Debug, Clone)]
pub struct PrivateSubject(pub String);

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivateLookupQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivateWebDetailQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
    include_trace: Option<bool>,
    include_profile: Option<bool>,
    /// `rows` (default) or `columns`, as for the public details.
    trace_format: Option<String>,
    /// Same semantics as the public web detail: `cursor` is the last seen
    /// trace timestamp and only strictly newer rows come back; `limit`
    /// caps the page (oldest first, clamped to `MAX_TRACE_LIMIT`). Without
    /// either the whole history is returned as before.
    cursor: Option<i64>,
    limit: Option<u64>,
}

impl PrivateWebDetailQuery {
    fn trace_filter(&self) -> Option<WebTraceFilter> {
        if self.cursor.is_none() && self.limit.is_none() {
            return None;
        }
        Some(WebTraceFilter {
            start_time: None,
            end_time: None,
            cursor: self.cursor,
            limit: self.limit.map(|limit| limit.clamp(1, MAX_TRACE_LIMIT)),
        })
    }
}

pub async fn require_subject(
    State(trust): State<Arc<ProxyTrust>>,
    mut req: Request,
    next: Next,
) -> Response {
    let subject = req
        .extensions()
        .get::<PrivateSubject>()
        .map(|subject| subject.0.clone())
        .or_else(|| {
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr);
            resolve_trusted_subject(req.headers(), &trust, peer)
        });
    let Some(subject) = subject else {
        return unauthorized().into_response();
    };

    req.extensions_mut().insert(PrivateSubject(subject));
    next.run(req).await
}

async fn require_bound_user(
    state: &AppState,
    subject: &PrivateSubject,
    owner: Option<&str>,
    server: SekaiServerRegion,
    user_id: &str,
) -> Result<(), ApiError> {
    let verifier = state.private_lookup().ok_or_else(|| {
        ApiError::ServiceUnavailable("private lookup verifier is not configured".into())
    })?;
    verifier
        .verify_bound_user(&subject.0, owner, server, user_id)
        .await
        .map_err(map_private_lookup_error)
}

fn map_private_lookup_error(err: PrivateLookupError) -> ApiError {
    match err {
        PrivateLookupError::NotConfigured => {
            ApiError::ServiceUnavailable("private lookup verifier is not configured".into())
        }
        PrivateLookupError::Unauthorized => ApiError::Unauthorized,
        PrivateLookupError::Forbidden => ApiError::Forbidden,
        PrivateLookupError::Upstream => {
            ApiError::ServiceUnavailable("private lookup verifier request failed".into())
        }
    }
}

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn latest_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let ranking = fetch_latest_ranking(&engine, event_id, &user_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if ranking.is_none() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserLatestRankingQueryResponseSchema {
        rank_data: ranking.map(RecordedRankData::Normal),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn latest_world_bloom_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let ranking =
        fetch_latest_world_bloom_ranking(&engine, event_id, &user_id, character_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if ranking.is_none() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserLatestRankingQueryResponseSchema {
        rank_data: ranking.map(RecordedRankData::WorldBloom),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn trace_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let _permit = state.query_limiter().acquire_trace(region).await?;
    let rankings = fetch_all_rankings(&engine, event_id, &user_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if rankings.is_empty() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings.into_iter().map(RecordedRankData::Normal).collect(),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn trace_world_bloom_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let _permit = state.query_limiter().acquire_trace(region).await?;
    let rankings =
        fetch_all_world_bloom_rankings(&engine, event_id, &user_id, character_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if rankings.is_empty() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings
            .into_iter()
            .map(RecordedRankData::WorldBloom)
            .collect(),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn web_total_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<PrivateWebDetailQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(state, server, event_id, None, user_id, query, subject).await
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn web_world_bloom_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<PrivateWebDetailQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        user_id,
        query,
        subject,
    )
    .await
}

async fn web_user_detail_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    user_id: String,
    query: PrivateWebDetailQuery,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let trace_format =
        TraceFormat::parse(query.trace_format.as_deref()).map_err(ApiError::BadRequest)?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let latest = match character_id {
        Some(character_id) => {
            fetch_latest_world_bloom_ranking(&engine, event_id, &user_id, character_id, mode)
                .await?
                .map(RecordedRankData::WorldBloom)
        }
        None => fetch_latest_ranking(&engine, event_id, &user_id, mode)
            .await?
            .map(RecordedRankData::Normal),
    };
    let tracked = latest.is_some();
    // The player's last row keeps its rank after they leave the tracked
    // ranks; unless they still hold that rank they are not ranked.
    let occupant = match latest.as_ref() {
        Some(rank_data) => {
            fetch_rank_item(
                &engine,
                event_id,
                character_id,
                rank_of_rank_data(rank_data),
                mode,
            )
            .await?
        }
        None => None,
    };
    let ranked = occupant
        .as_ref()
        .is_some_and(|item| user_id_of(&item.rank_data) == user_id);
    let current = latest.filter(|_| ranked);
    let rank = current.as_ref().map(rank_of_rank_data);
    let (previous, next) = tokio::try_join!(
        async {
            match rank.filter(|rank| *rank > 1) {
                Some(rank) => {
                    fetch_rank_item(&engine, event_id, character_id, rank - 1, mode).await
                }
                None => Ok(None),
            }
        },
        async {
            match rank {
                Some(rank) => {
                    fetch_rank_item(&engine, event_id, character_id, rank + 1, mode).await
                }
                None => Ok(None),
            }
        },
    )?;
    let current = current.map(|rank_data| WebRankingItemSchema {
        rank_data,
        user_data: None,
    });
    let player_trace = if query.include_trace.unwrap_or(false) {
        let _permit = state.query_limiter().acquire_trace(region).await?;
        match (query.trace_filter(), character_id) {
            (Some(filter), Some(character_id)) => {
                search_world_bloom_user_trace(
                    &engine,
                    event_id,
                    character_id,
                    &user_id,
                    &filter,
                    mode,
                )
                .await?
            }
            (Some(filter), None) => {
                search_user_trace(&engine, event_id, &user_id, &filter, mode).await?
            }
            (None, Some(character_id)) => {
                fetch_all_world_bloom_rankings(&engine, event_id, &user_id, character_id, mode)
                    .await?
                    .into_iter()
                    .map(RecordedRankData::WorldBloom)
                    .collect()
            }
            (None, None) => fetch_all_rankings(&engine, event_id, &user_id, mode)
                .await?
                .into_iter()
                .map(RecordedRankData::Normal)
                .collect(),
        }
    } else {
        Vec::new()
    };
    let profile = if query.include_profile.unwrap_or(false) {
        get_user_data(&engine, event_id, &user_id, mode).await?
    } else {
        None
    };
    // A cursor poll with nothing newer is an empty increment for a player
    // the event has tracked; only a player it never saw is a 404.
    let empty = current.is_none() && player_trace.is_empty() && profile.is_none();
    if empty && !(query.cursor.is_some() && tracked) {
        return Err(ApiError::NotFound);
    }
    let player_trace = TracePayload::encode(player_trace, trace_format).map_err(|err| {
        tracing::error!(%err, "trace columns encode error");
        ApiError::ServiceUnavailable("json encode error".into())
    })?;
    Ok(Json(WebUserDetailResponseSchema {
        meta: LeaderboardMetaSchema {
            server,
            event_id,
            scope: match character_id {
                Some(character_id) => format!("world-bloom/{character_id}"),
                None => "total".to_owned(),
            },
            character_id,
            fetched_at: chrono::Utc::now().timestamp(),
        },
        subject: None,
        ranked,
        current,
        previous,
        next,
        player_trace,
        profile,
    }))
}

async fn fetch_rank_item(
    engine: &crate::db::engine::DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    rank: i64,
    mode: crate::db::query::user::PublicUserIdMode,
) -> Result<Option<WebRankingItemSchema>, ApiError> {
    let rank_data = match character_id {
        Some(character_id) => {
            fetch_latest_world_bloom_ranking_by_rank(engine, event_id, rank, character_id, mode)
                .await?
                .map(RecordedRankData::WorldBloom)
        }
        None => fetch_latest_ranking_by_rank(engine, event_id, rank, mode)
            .await?
            .map(RecordedRankData::Normal),
    };
    Ok(rank_data.map(|rank_data| WebRankingItemSchema {
        rank_data,
        user_data: None,
    }))
}

fn user_id_of(rank_data: &RecordedRankData) -> &str {
    match rank_data {
        RecordedRankData::Normal(data) => &data.user_id,
        RecordedRankData::WorldBloom(data) => &data.user_id,
    }
}

fn rank_of_rank_data(rank_data: &RecordedRankData) -> i64 {
    match rank_data {
        RecordedRankData::Normal(data) => data.rank,
        RecordedRankData::WorldBloom(data) => data.rank,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::private_lookup::PrivateLookupVerifier;
    use crate::api::private_lookup::tests::spawn_toolbox;
    use crate::api::realtime::RealtimeHub;
    use crate::api::ws_ticket::WsTicketStore;
    use crate::config::{ApiQueryConfig, ToolboxConfig};
    use crate::db::query::web::tests::{
        seed_normal_event_with_history, seed_player_pushed_out,
        seed_world_bloom_event_with_history, sqlite_engine,
    };
    use crate::db::schema::create_event_tables;
    use crate::privacy::UidAnonymizer;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::get;
    use std::collections::HashMap;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    const NORMAL_EVENT: i64 = 831;
    const WORLD_BLOOM_EVENT: i64 = 832;

    const BINDINGS: &str = r#"{"updatedData":{"kratosIdentityId":"identity-1","gameAccountBindings":[{"server":"jp","userId":100}]}}"#;

    async fn test_state(with_verifier: bool) -> AppState {
        test_state_bound(with_verifier.then_some(BINDINGS)).await
    }

    async fn test_state_bound(bindings: Option<&'static str>) -> AppState {
        let engine = sqlite_engine().await;
        create_event_tables(&engine, SekaiServerRegion::Jp, NORMAL_EVENT, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, NORMAL_EVENT).await;
        create_event_tables(&engine, SekaiServerRegion::Jp, WORLD_BLOOM_EVENT, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_history(&engine, WORLD_BLOOM_EVENT).await;
        let verifier = match bindings {
            Some(bindings) => {
                let base_url = spawn_toolbox(bindings, Arc::new(Mutex::new(Vec::new()))).await;
                PrivateLookupVerifier::from_config(&ToolboxConfig {
                    base_url,
                    ..ToolboxConfig::default()
                })
            }
            None => None,
        };
        AppState::new(
            HashMap::from([(SekaiServerRegion::Jp, Arc::new(engine))]),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), [SekaiServerRegion::Jp]),
            UidAnonymizer::enabled("test-salt"),
            verifier,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
    }

    fn lookup_query() -> PrivateLookupQuery {
        PrivateLookupQuery {
            owner: Some("identity-1".into()),
            owner_id: None,
        }
    }

    fn detail_query() -> PrivateWebDetailQuery {
        PrivateWebDetailQuery {
            owner: None,
            owner_id: Some("identity-1".into()),
            include_trace: Some(true),
            include_profile: Some(true),
            trace_format: None,
            cursor: None,
            limit: None,
        }
    }

    fn timestamps(rows: &[RecordedRankData]) -> Vec<i64> {
        rows.iter()
            .map(|row| match row {
                RecordedRankData::Normal(data) => data.timestamp,
                RecordedRankData::WorldBloom(data) => data.timestamp,
            })
            .collect()
    }

    fn subject() -> axum::Extension<PrivateSubject> {
        axum::Extension(PrivateSubject("identity-1".into()))
    }

    #[tokio::test]
    async fn private_lookup_handlers_return_raw_user_data() {
        let state = test_state(true).await;
        let latest = latest_by_user(
            State(state.clone()),
            Path(("jp".into(), NORMAL_EVENT, "100".into())),
            Query(lookup_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert!(latest.rank_data.is_some());
        assert_eq!(latest.user_data.unwrap().user_id, "100");

        let world = latest_world_bloom_by_user(
            State(state.clone()),
            Path(("jp".into(), WORLD_BLOOM_EVENT, 17, "100".into())),
            Query(lookup_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert!(matches!(
            world.rank_data,
            Some(RecordedRankData::WorldBloom(_))
        ));

        let trace = trace_by_user(
            State(state.clone()),
            Path(("jp".into(), NORMAL_EVENT, "100".into())),
            Query(lookup_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(trace.rank_data.len(), 2);

        let world_trace = trace_world_bloom_by_user(
            State(state),
            Path(("jp".into(), WORLD_BLOOM_EVENT, 17, "100".into())),
            Query(lookup_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(world_trace.rank_data.len(), 2);
    }

    #[tokio::test]
    async fn private_web_details_cover_total_and_world_bloom() {
        let state = test_state(true).await;
        let total = web_total_user_detail(
            State(state.clone()),
            Path(("jp".into(), NORMAL_EVENT, "100".into())),
            Query(detail_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert!(total.current.is_some());
        assert!(total.next.is_some());
        assert_eq!(total.player_trace.rows().len(), 2);
        assert_eq!(total.profile.unwrap().user_id, "100");

        let world = web_world_bloom_user_detail(
            State(state),
            Path(("jp".into(), WORLD_BLOOM_EVENT, 17, "100".into())),
            Query(detail_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert!(world.ranked);
        assert!(world.current.is_some());
        assert!(world.next.is_some());
        assert_eq!(world.meta.character_id, Some(17));
    }

    #[tokio::test]
    async fn private_web_details_offer_the_columns_trace_format() {
        let state = test_state(true).await;
        let rows = web_total_user_detail(
            State(state.clone()),
            Path(("jp".into(), NORMAL_EVENT, "100".into())),
            Query(detail_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        let columns = web_total_user_detail(
            State(state.clone()),
            Path(("jp".into(), NORMAL_EVENT, "100".into())),
            Query(PrivateWebDetailQuery {
                trace_format: Some("columns".into()),
                ..detail_query()
            }),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(columns.player_trace.format(), TraceFormat::Columns);
        assert_eq!(
            sonic_rs::to_string(&columns.player_trace.rows()).unwrap(),
            sonic_rs::to_string(&rows.player_trace.rows()).unwrap()
        );
        let json = sonic_rs::to_string(&columns).unwrap();
        assert!(
            json.contains(r#""playerTrace":{"format":"columns""#),
            "{json}"
        );
        assert!(json.contains(r#""users":["100"]"#), "{json}");
        assert!(matches!(
            web_total_user_detail(
                State(state),
                Path(("jp".into(), NORMAL_EVENT, "100".into())),
                Query(PrivateWebDetailQuery {
                    trace_format: Some("csv".into()),
                    ..detail_query()
                }),
                subject(),
            )
            .await,
            Err(ApiError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn private_web_details_of_players_who_left_the_ranks_are_not_ranked() {
        let state = test_state(true).await;
        let (_, engine) = resolve_region_engine(&state, "jp").unwrap();
        seed_player_pushed_out(&engine, NORMAL_EVENT, None, 1).await;
        seed_player_pushed_out(&engine, WORLD_BLOOM_EVENT, Some(17), 1).await;
        let total = web_total_user_detail(
            State(state.clone()),
            Path(("jp".into(), NORMAL_EVENT, "100".into())),
            Query(detail_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        let world = web_world_bloom_user_detail(
            State(state),
            Path(("jp".into(), WORLD_BLOOM_EVENT, 17, "100".into())),
            Query(detail_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        for detail in [total, world] {
            assert!(!detail.ranked);
            assert!(detail.current.is_none());
            assert!(detail.previous.is_none());
            assert!(detail.next.is_none());
            assert_eq!(detail.player_trace.rows().len(), 2);
            assert!(
                detail
                    .player_trace
                    .rows()
                    .iter()
                    .all(|row| user_id_of(row) == "100")
            );
            assert_eq!(detail.profile.unwrap().user_id, "100");
        }
    }

    #[tokio::test]
    async fn private_web_details_page_the_trace_with_cursor_and_limit() {
        let state = test_state(true).await;
        let total_path = || Path(("jp".into(), NORMAL_EVENT, "100".into()));
        let world_path = || Path(("jp".into(), WORLD_BLOOM_EVENT, 17, "100".into()));
        let full = web_total_user_detail(
            State(state.clone()),
            total_path(),
            Query(detail_query()),
            subject(),
        )
        .await
        .unwrap()
        .0;
        let all = full.player_trace.rows();
        let stamps = timestamps(&all);
        assert_eq!(stamps.len(), 2);
        assert!(stamps[0] < stamps[1]);

        // An explicit (unreached) limit takes the filtered path and yields
        // the same bytes as the unfiltered history.
        let mut query = detail_query();
        query.limit = Some(u64::MAX);
        let limited =
            web_total_user_detail(State(state.clone()), total_path(), Query(query), subject())
                .await
                .unwrap()
                .0;
        assert_eq!(
            sonic_rs::to_vec(&limited.player_trace).unwrap(),
            sonic_rs::to_vec(&full.player_trace).unwrap()
        );

        // `limit` pages from the oldest row; `cursor` continues after it.
        let mut query = detail_query();
        query.limit = Some(1);
        query.include_profile = None;
        let first =
            web_total_user_detail(State(state.clone()), total_path(), Query(query), subject())
                .await
                .unwrap()
                .0;
        assert_eq!(timestamps(&first.player_trace.rows()), vec![stamps[0]]);
        let mut query = detail_query();
        query.cursor = Some(stamps[0]);
        query.include_profile = None;
        let rest =
            web_total_user_detail(State(state.clone()), total_path(), Query(query), subject())
                .await
                .unwrap()
                .0;
        assert_eq!(timestamps(&rest.player_trace.rows()), vec![stamps[1]]);
        assert!(rest.current.is_some());
        assert!(rest.profile.is_none());

        // Nothing newer than the cursor is an empty increment, not a 404,
        // for a tracked player -- ranked or pushed out of the ranks.
        let mut query = detail_query();
        query.cursor = Some(stamps[1]);
        query.include_profile = None;
        let none =
            web_total_user_detail(State(state.clone()), total_path(), Query(query), subject())
                .await
                .unwrap()
                .0;
        assert!(none.player_trace.is_empty());
        assert!(none.current.is_some());
        let (_, engine) = resolve_region_engine(&state, "jp").unwrap();
        seed_player_pushed_out(&engine, NORMAL_EVENT, None, 1).await;
        seed_player_pushed_out(&engine, WORLD_BLOOM_EVENT, Some(17), 1).await;
        let mut query = detail_query();
        query.cursor = Some(i64::MAX - 1);
        query.include_profile = None;
        let pushed_out =
            web_total_user_detail(State(state.clone()), total_path(), Query(query), subject())
                .await
                .unwrap()
                .0;
        assert!(!pushed_out.ranked);
        assert!(pushed_out.current.is_none());
        assert!(pushed_out.player_trace.is_empty());
        let mut query = detail_query();
        query.cursor = Some(i64::MAX - 1);
        query.include_profile = None;
        let world = web_world_bloom_user_detail(
            State(state.clone()),
            world_path(),
            Query(query),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert!(!world.ranked && world.player_trace.is_empty());
        let mut query = detail_query();
        query.cursor = Some(stamps[0]);
        query.limit = Some(1);
        let world = web_world_bloom_user_detail(
            State(state.clone()),
            world_path(),
            Query(query),
            subject(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(world.player_trace.rows().len(), 1);
        assert!(timestamps(&world.player_trace.rows())[0] > stamps[0]);

        // Without a cursor the old 404 rule stands, and a player the event
        // never saw is a 404 even on a cursor poll.
        let mut query = detail_query();
        query.include_trace = None;
        query.include_profile = None;
        assert!(matches!(
            web_total_user_detail(State(state.clone()), total_path(), Query(query), subject())
                .await,
            Err(ApiError::NotFound)
        ));
        let stranger = test_state_bound(Some(
            r#"{"updatedData":{"kratosIdentityId":"identity-1","gameAccountBindings":[{"server":"jp","userId":999}]}}"#,
        ))
        .await;
        let mut query = detail_query();
        query.cursor = Some(0);
        assert!(matches!(
            web_total_user_detail(
                State(stranger),
                Path(("jp".into(), NORMAL_EVENT, "999".into())),
                Query(query),
                subject()
            )
            .await,
            Err(ApiError::NotFound)
        ));
    }

    #[tokio::test]
    async fn private_auth_and_error_mapping_cover_failure_paths() {
        let (trust, invalid) = ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
        assert!(invalid.is_empty());
        let trust = Arc::new(trust);
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .route_layer(middleware::from_fn_with_state(trust, require_subject));
        let unauthorized = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), axum::http::StatusCode::UNAUTHORIZED);

        let mut request = Request::builder().uri("/").body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(PrivateSubject("identity-1".into()));
        assert!(app.oneshot(request).await.unwrap().status().is_success());

        assert!(matches!(
            map_private_lookup_error(PrivateLookupError::NotConfigured),
            ApiError::ServiceUnavailable(_)
        ));
        assert!(matches!(
            map_private_lookup_error(PrivateLookupError::Unauthorized),
            ApiError::Unauthorized
        ));
        assert!(matches!(
            map_private_lookup_error(PrivateLookupError::Forbidden),
            ApiError::Forbidden
        ));
        assert!(matches!(
            map_private_lookup_error(PrivateLookupError::Upstream),
            ApiError::ServiceUnavailable(_)
        ));

        let state = test_state(false).await;
        let error = require_bound_user(
            &state,
            &PrivateSubject("identity-1".into()),
            None,
            SekaiServerRegion::Jp,
            "100",
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::ServiceUnavailable(_)));
    }
}
