use serde::Deserialize;

use crate::api::error::ApiError;
use crate::api::extract::{prepare_user_id_mode, resolve_region_engine};
use crate::api::handler::web::{build_overview, build_world_bloom_overview, cached_overview_bytes};
use crate::api::json::{Json, RawJson};
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
    limit: Option<u64>,
}

pub(crate) async fn web_overview_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: OverviewQuery,
    cache_prefix: &str,
) -> Result<RawJson, ApiError> {
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
    let response =
        cached_overview_bytes(&state, &cache_server, event_id, suffix, at.is_some(), fetch).await?;
    Ok(RawJson(response))
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
            SubjectTraceQuery {
                subject_type: Some("rank".to_owned()),
                include_current: Some(true),
                start_time: None,
                end_time: None,
                cursor: None,
                limit: query.limit,
            },
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
            SubjectTraceQuery {
                subject_type: Some("user".to_owned()),
                include_current: Some(true),
                start_time: None,
                end_time: None,
                cursor: None,
                limit: query.limit,
            },
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
            SubjectTraceQuery {
                subject_type: Some("user".to_owned()),
                include_current: Some(true),
                start_time: None,
                end_time: None,
                cursor: None,
                limit: query.limit,
            },
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
