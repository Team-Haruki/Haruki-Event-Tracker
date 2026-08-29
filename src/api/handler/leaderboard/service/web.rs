use serde::Deserialize;

use crate::api::error::ApiError;
use crate::api::extract::{prepare_user_id_mode, resolve_region_engine};
use crate::api::handler::web::{build_overview, build_world_bloom_overview, cached_overview_bytes};
use crate::api::json::{EncodedJson, Json};
use crate::api::state::AppState;
use crate::model::api::{
    LeaderboardOverviewSchema, WebRankDetailResponseSchema, WebUserDetailResponseSchema,
};

use super::snapshot::{SnapshotBuildRequest, build_rank_snapshots_response};
use super::trace::{SubjectTraceQuery, build_subject_trace_response};
use super::util::{interval_seconds, meta, positive_timestamp, rank_of_item, user_id_of_rank_data};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewQuery {
    interval: Option<i64>,
    at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDetailQuery {
    interval: Option<i64>,
    at: Option<i64>,
    include_trace: Option<bool>,
    include_player_trace: Option<bool>,
    include_profile: Option<bool>,
    cursor: Option<i64>,
    limit: Option<u64>,
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
    let suffix = match character_id {
        Some(character_id) => {
            format!("{cache_prefix}:wb:{character_id}:overview:interval={interval}:at={at:?}")
        }
        None => format!("{cache_prefix}:total:overview:interval={interval}:at={at:?}"),
    };
    let cache_server = server.clone();
    let fetch = async {
        let (region, engine) = resolve_region_engine(&state, &server)?;
        let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
        let overview = match character_id {
            Some(character_id) => {
                build_world_bloom_overview(&engine, event_id, character_id, mode, interval, at)
                    .await?
            }
            None => build_overview(&engine, event_id, mode, interval, at).await?,
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

pub(crate) async fn web_user_detail_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    user_id: String,
    query: WebDetailQuery,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    let trace = build_subject_trace_response(
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
    )
    .await?;
    let current = trace.current;
    let rank = current
        .as_ref()
        .and_then(rank_of_item)
        .ok_or(ApiError::NotFound)?;
    let snapshot = build_rank_snapshots_response(
        state.clone(),
        server.clone(),
        event_id,
        character_id,
        SnapshotBuildRequest {
            ranks: vec![rank],
            include_adjacent: true,
            include_metrics: false,
            interval: interval_seconds(query.interval),
            at: positive_timestamp(query.at),
            cache_prefix: "web:v2",
        },
    )
    .await?;
    let item = snapshot
        .items
        .into_iter()
        .find(|item| item.rank == rank)
        .ok_or(ApiError::NotFound)?;
    let player_trace = if query.include_trace.unwrap_or(false) {
        build_subject_trace_response(
            state,
            server,
            event_id,
            character_id,
            user_id,
            detail_trace_query(&query, "user"),
            "web:v2",
        )
        .await?
        .rank_data
    } else {
        Vec::new()
    };
    Ok(Json(WebUserDetailResponseSchema {
        meta: snapshot.meta,
        current: item.current,
        previous: item.previous,
        next: item.next,
        player_trace,
        profile: query
            .include_profile
            .unwrap_or(false)
            .then_some(trace.user_data)
            .flatten(),
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
            UidAnonymizer::disabled(),
            None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
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
            "100".into(),
            detail_query(),
        )
        .await
        .unwrap()
        .0;
        assert!(detail.current.is_some());
        assert_eq!(detail.profile.unwrap().name, "Alpha");
        assert_eq!(detail.player_trace.len(), 2);

        let mut without_trace = detail_query();
        without_trace.include_trace = Some(false);
        without_trace.include_profile = Some(false);
        let world = web_user_detail_for_scope(
            state,
            "jp".into(),
            WORLD_BLOOM_EVENT,
            Some(17),
            "100".into(),
            without_trace,
        )
        .await
        .unwrap()
        .0;
        assert!(world.current.is_some());
        assert!(world.profile.is_none());
        assert!(world.player_trace.is_empty());
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
        };

        let trace_query = detail_trace_query(&query, "user");

        assert_eq!(trace_query.subject_type.as_deref(), Some("user"));
        assert_eq!(trace_query.cursor, Some(1_786_726_540));
        assert_eq!(trace_query.limit, Some(5_000));
    }
}
