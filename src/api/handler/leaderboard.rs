use std::collections::{BTreeMap, BTreeSet};

use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::api::cache::CacheTtl;
use crate::api::error::ApiError;
use crate::api::extract::{parse_rank_query, prepare_user_id_mode, resolve_region_engine};
use crate::api::handler::web::{
    UserSearchQuery, build_overview, build_world_bloom_overview, cached_overview_bytes,
    cached_trace,
};
use crate::api::json::{Json, RawJson};
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::db::query::growth::{
    fetch_ranking_score_growths, fetch_world_bloom_ranking_score_growths,
};
use crate::db::query::ranking::{fetch_all_rankings, fetch_latest_ranking_by_rank};
use crate::db::query::user::{PublicUserIdMode, get_user_data};
use crate::db::query::web::{
    WebRankingFilter, WebTraceFilter, search_rank_trace, search_ranking_rows, search_user_trace,
    search_world_bloom_rank_trace, search_world_bloom_ranking_rows, search_world_bloom_user_trace,
};
use crate::db::query::world_bloom::fetch_latest_world_bloom_ranking_by_rank;
use crate::model::api::{
    CloudCheckRoomResponseSchema, CloudLineResponseSchema, CloudRankInfoSchema,
    CloudRankQueryResponseSchema, CloudSpeedResponseSchema, CloudTraceResponseSchema,
    LeaderboardMetaSchema, LeaderboardOverviewSchema, RankSnapshotSchema,
    RankSnapshotsResponseSchema, RankingScoreGrowthSchema, RecordedRankData,
    SubjectTraceMetaSchema, SubjectTraceResponseSchema, WebRankDetailResponseSchema,
    WebRankingItemSchema, WebUserDetailResponseSchema, WebUserSearchPageSchema,
};

const DEFAULT_INTERVAL_SECONDS: i64 = 3600;
const MAX_TRACE_LIMIT: u64 = 10_000;
const CLOUD_TRACE_METRICS_LIMIT: u64 = 5_000;
const TRACKER_REALTIME_TAIL_MAX_LAG_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewQuery {
    interval: Option<i64>,
    at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubjectTraceQuery {
    subject_type: Option<String>,
    include_current: Option<bool>,
    start_time: Option<i64>,
    end_time: Option<i64>,
    cursor: Option<i64>,
    limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudQuery {
    user_id: Option<String>,
    interval: Option<i64>,
    unit_seconds: Option<i64>,
    include_adjacent: Option<bool>,
    skip_missing: Option<bool>,
    subject_type: Option<String>,
    subject: Option<String>,
    limit: Option<u64>,
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

struct SnapshotBuildRequest {
    ranks: Vec<i64>,
    include_adjacent: bool,
    include_metrics: bool,
    interval: i64,
    at: Option<i64>,
    cache_prefix: &'static str,
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id))]
pub async fn cloud_total_query(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudRankQueryResponseSchema>, ApiError> {
    cloud_query_for_scope(state, server, event_id, None, query, raw_query, true).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn cloud_world_bloom_query(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudRankQueryResponseSchema>, ApiError> {
    cloud_query_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        raw_query,
        true,
    )
    .await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id))]
pub async fn cloud_total_check_room(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudCheckRoomResponseSchema>, ApiError> {
    cloud_check_room_for_scope(state, server, event_id, None, query, raw_query).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn cloud_world_bloom_check_room(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudCheckRoomResponseSchema>, ApiError> {
    cloud_check_room_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        raw_query,
    )
    .await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id))]
pub async fn cloud_total_line(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudLineResponseSchema>, ApiError> {
    cloud_line_for_scope(state, server, event_id, None, query, raw_query).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn cloud_world_bloom_line(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudLineResponseSchema>, ApiError> {
    cloud_line_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        raw_query,
    )
    .await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id))]
pub async fn cloud_total_speed(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudSpeedResponseSchema>, ApiError> {
    cloud_speed_for_scope(state, server, event_id, None, query, raw_query).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn cloud_world_bloom_speed(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudSpeedResponseSchema>, ApiError> {
    cloud_speed_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        raw_query,
    )
    .await
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn cloud_total_trace(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
) -> Result<Json<CloudTraceResponseSchema>, ApiError> {
    cloud_trace_for_scope(state, server, event_id, None, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn cloud_world_bloom_trace(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<CloudQuery>,
) -> Result<Json<CloudTraceResponseSchema>, ApiError> {
    cloud_trace_for_scope(state, server, event_id, Some(character_id), query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn web_total_overview(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<OverviewQuery>,
) -> Result<RawJson, ApiError> {
    web_overview_for_scope(state, server, event_id, None, query, "web:v2").await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn web_world_bloom_overview(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<OverviewQuery>,
) -> Result<RawJson, ApiError> {
    web_overview_for_scope(state, server, event_id, Some(character_id), query, "web:v2").await
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn web_total_replay_overview(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<OverviewQuery>,
) -> Result<RawJson, ApiError> {
    web_overview_for_scope(state, server, event_id, None, query, "web:v2:replay").await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn web_world_bloom_replay_overview(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<OverviewQuery>,
) -> Result<RawJson, ApiError> {
    web_overview_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        "web:v2:replay",
    )
    .await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, rank))]
pub async fn web_total_rank_detail(
    State(state): State<AppState>,
    Path((server, event_id, rank)): Path<(String, i64, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebRankDetailResponseSchema>, ApiError> {
    web_rank_detail_for_scope(state, server, event_id, None, rank, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id, rank))]
pub async fn web_world_bloom_rank_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, rank)): Path<(String, i64, i64, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebRankDetailResponseSchema>, ApiError> {
    web_rank_detail_for_scope(state, server, event_id, Some(character_id), rank, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, user_id))]
pub async fn web_total_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(state, server, event_id, None, user_id, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id, user_id))]
pub async fn web_world_bloom_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(state, server, event_id, Some(character_id), user_id, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn web_world_bloom_users(
    State(state): State<AppState>,
    Path((server, event_id, _character_id)): Path<(String, i64, i64)>,
    Query(query): Query<UserSearchQuery>,
) -> Result<Json<WebUserSearchPageSchema>, ApiError> {
    crate::api::handler::web::users(State(state), Path((server, event_id)), Query(query)).await
}

async fn web_overview_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: OverviewQuery,
    cache_prefix: &str,
) -> Result<RawJson, ApiError> {
    let interval = interval_seconds(query.interval);
    let at = positive_timestamp(query.at);
    let end_time = at.unwrap_or_else(|| Utc::now().timestamp());
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

async fn cloud_query_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
    raw_query: Option<String>,
    include_round_metrics: bool,
) -> Result<Json<CloudRankQueryResponseSchema>, ApiError> {
    let ranks = parse_rank_query(raw_query.as_deref()).unwrap_or_default();
    let include_adjacent = query.include_adjacent.unwrap_or(true);
    let snapshots = if !ranks.is_empty() {
        build_rank_snapshots_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            SnapshotBuildRequest {
                ranks,
                include_adjacent,
                include_metrics: true,
                interval: interval_seconds(query.interval),
                at: None,
                cache_prefix: "cloud:v2",
            },
        )
        .await?
    } else if let Some(user_id) = query.user_id.as_deref().filter(|id| !id.trim().is_empty()) {
        let subject = build_subject_trace_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            user_id.to_owned(),
            SubjectTraceQuery {
                subject_type: Some("user".to_owned()),
                include_current: Some(true),
                start_time: None,
                end_time: None,
                cursor: None,
                limit: Some(1),
            },
            "cloud:v2",
        )
        .await?;
        let Some(current) = subject.current else {
            return Err(ApiError::NotFound);
        };
        let rank = rank_of_item(&current).ok_or(ApiError::NotFound)?;
        build_rank_snapshots_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            SnapshotBuildRequest {
                ranks: vec![rank],
                include_adjacent,
                include_metrics: true,
                interval: interval_seconds(query.interval),
                at: None,
                cache_prefix: "cloud:v2",
            },
        )
        .await?
    } else {
        return Err(ApiError::BadRequest("rank or userId is required".into()));
    };
    let skip_missing = query.skip_missing.unwrap_or(false);
    let mut ranks = cloud_infos_from_snapshots(&snapshots, skip_missing)?;
    if include_round_metrics {
        enrich_cloud_rank_infos_with_trace_metrics(
            &state,
            &server,
            event_id,
            character_id,
            &mut ranks,
        )
        .await;
    }
    let first = snapshots.items.first();
    Ok(Json(CloudRankQueryResponseSchema {
        meta: snapshots.meta,
        ranks,
        previous: first
            .and_then(|item| item.previous.as_ref())
            .and_then(cloud_info_from_item),
        next: first
            .and_then(|item| item.next.as_ref())
            .and_then(cloud_info_from_item),
    }))
}

async fn cloud_check_room_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
    raw_query: Option<String>,
) -> Result<Json<CloudCheckRoomResponseSchema>, ApiError> {
    let response = cloud_query_for_scope(
        state,
        server,
        event_id,
        character_id,
        query,
        raw_query,
        true,
    )
    .await?;
    let response = response.0;
    let rank = response
        .ranks
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    Ok(Json(CloudCheckRoomResponseSchema {
        meta: response.meta,
        rank,
        previous: response.previous,
        next: response.next,
    }))
}

async fn cloud_line_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    mut query: CloudQuery,
    raw_query: Option<String>,
) -> Result<Json<CloudLineResponseSchema>, ApiError> {
    query.include_adjacent = Some(false);
    let response = cloud_query_for_scope(
        state,
        server,
        event_id,
        character_id,
        query,
        raw_query,
        false,
    )
    .await?;
    let response = response.0;
    Ok(Json(CloudLineResponseSchema {
        meta: response.meta,
        ranks: response
            .ranks
            .into_iter()
            .map(|mut rank| {
                rank.name.clear();
                rank
            })
            .collect(),
    }))
}

async fn cloud_speed_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
    raw_query: Option<String>,
) -> Result<Json<CloudSpeedResponseSchema>, ApiError> {
    let interval = interval_seconds(query.interval);
    let unit_seconds = query.unit_seconds.unwrap_or(3600).clamp(1, 86_400);
    let snapshots = build_rank_snapshots_response(
        state,
        server,
        event_id,
        character_id,
        SnapshotBuildRequest {
            ranks: parse_rank_query(raw_query.as_deref())?,
            include_adjacent: false,
            include_metrics: true,
            interval,
            at: None,
            cache_prefix: "cloud:v2",
        },
    )
    .await?;
    let speeds = cloud_infos_from_snapshots(&snapshots, query.skip_missing.unwrap_or(false))?
        .into_iter()
        .map(|mut info| {
            if let Some(speed) = info.speed {
                info.speed = Some(speed * unit_seconds / interval.max(1));
            }
            info.speed_window = Some(interval);
            info
        })
        .collect();
    Ok(Json(CloudSpeedResponseSchema {
        meta: snapshots.meta,
        speeds,
        interval_seconds: interval,
        unit_seconds,
    }))
}

async fn cloud_trace_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
) -> Result<Json<CloudTraceResponseSchema>, ApiError> {
    let subject_type = query.subject_type.unwrap_or_else(|| "user".to_owned());
    let subject = query
        .subject
        .or(query.user_id)
        .ok_or_else(|| ApiError::BadRequest("subject is required".into()))?;
    let trace = build_subject_trace_response(
        state,
        server,
        event_id,
        character_id,
        subject,
        SubjectTraceQuery {
            subject_type: Some(subject_type),
            include_current: Some(true),
            start_time: None,
            end_time: None,
            cursor: None,
            limit: query.limit,
        },
        "cloud:v2",
    )
    .await?;
    let name = trace
        .user_data
        .as_ref()
        .map(|user| user.name.clone())
        .or_else(|| {
            trace
                .current
                .as_ref()
                .and_then(|current| current.user_data.as_ref().map(|user| user.name.clone()))
        })
        .unwrap_or_default();
    let rank_data = trace
        .rank_data
        .iter()
        .filter_map(|data| cloud_info_from_rank_data(data, Some(name.as_str()), None))
        .collect();
    Ok(Json(CloudTraceResponseSchema {
        meta: trace.meta,
        subject: trace.subject,
        rank_data,
    }))
}

async fn build_rank_snapshots_response(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    request: SnapshotBuildRequest,
) -> Result<RankSnapshotsResponseSchema, ApiError> {
    let SnapshotBuildRequest {
        ranks,
        include_adjacent,
        include_metrics,
        interval,
        at,
        cache_prefix,
    } = request;
    let end_time = at.unwrap_or_else(|| Utc::now().timestamp());
    let mut requested = BTreeSet::new();
    for rank in &ranks {
        requested.insert(*rank);
        if include_adjacent {
            if *rank > 1 {
                requested.insert(*rank - 1);
            }
            requested.insert(*rank + 1);
        }
    }
    let all_ranks = requested.into_iter().collect::<Vec<_>>();
    let suffix = match character_id {
        Some(character_id) => format!(
            "{cache_prefix}:wb:{character_id}:snapshots:ranks={}:adj={include_adjacent}:metrics={include_metrics}:interval={interval}:at={at:?}",
            join_ranks(&ranks)
        ),
        None => format!(
            "{cache_prefix}:total:snapshots:ranks={}:adj={include_adjacent}:metrics={include_metrics}:interval={interval}:at={at:?}",
            join_ranks(&ranks)
        ),
    };
    let cache_server = server.clone();
    let fetch = async {
        let (region, engine) = resolve_region_engine(&state, &server)?;
        let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
        let current =
            fetch_snapshot_items(&engine, event_id, character_id, &all_ranks, mode, at).await?;
        let metrics = if include_metrics {
            fetch_snapshot_metrics(
                &engine,
                event_id,
                character_id,
                &ranks,
                end_time - interval,
                Some(end_time),
            )
            .await?
        } else {
            BTreeMap::new()
        };
        let mut items = Vec::with_capacity(ranks.len());
        for rank in ranks {
            let current_item = current.get(&rank).cloned();
            if current_item.is_none() {
                continue;
            }
            items.push(RankSnapshotSchema {
                rank,
                current: current_item,
                previous: (include_adjacent && rank > 1)
                    .then(|| current.get(&(rank - 1)).cloned())
                    .flatten(),
                next: include_adjacent
                    .then(|| current.get(&(rank + 1)).cloned())
                    .flatten(),
                metrics: metrics.get(&rank).cloned(),
            });
        }
        if items.is_empty() {
            return Err(ApiError::NotFound);
        }
        Ok(RankSnapshotsResponseSchema {
            meta: meta(&server, event_id, character_id, end_time),
            items,
            interval_seconds: interval,
            window_start: end_time - interval,
            window_end: end_time,
        })
    };
    cached_snapshot(&state, &cache_server, event_id, suffix, fetch).await
}

async fn web_rank_detail_for_scope(
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

async fn web_user_detail_for_scope(
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

async fn build_subject_trace_response(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    subject: String,
    query: SubjectTraceQuery,
    cache_prefix: &str,
) -> Result<SubjectTraceResponseSchema, ApiError> {
    let subject_type = query.subject_type.as_deref().unwrap_or("user");
    let include_current = query.include_current.unwrap_or(true);
    let filter = WebTraceFilter {
        start_time: query.start_time,
        end_time: query.end_time,
        cursor: query.cursor,
        limit: query
            .limit
            .unwrap_or(MAX_TRACE_LIMIT)
            .clamp(1, MAX_TRACE_LIMIT),
    };
    let suffix = match character_id {
        Some(character_id) => format!(
            "{cache_prefix}:wb:{character_id}:subject:{subject_type}:{subject}:current={include_current}:start={:?}:end={:?}:cursor={:?}:limit={}",
            filter.start_time, filter.end_time, filter.cursor, filter.limit
        ),
        None => format!(
            "{cache_prefix}:total:subject:{subject_type}:{subject}:current={include_current}:start={:?}:end={:?}:cursor={:?}:limit={}",
            filter.start_time, filter.end_time, filter.cursor, filter.limit
        ),
    };
    let cache_server = server.clone();
    let fetch = async {
        let (region, engine) = resolve_region_engine(&state, &server)?;
        let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
        let (user_id, resolved_rank, current, subject_kind) = resolve_subject(
            &engine,
            event_id,
            character_id,
            &subject,
            subject_type,
            mode,
            include_current,
        )
        .await?;
        let limiter = state.query_limiter().clone();
        let _permit = limiter.acquire_trace(region).await?;
        let rank_data = match character_id {
            Some(character_id) => match subject_kind {
                SubjectKind::Rank => {
                    let rank = resolved_rank.ok_or_else(|| {
                        ApiError::ServiceUnavailable("rank subject has no resolved rank".into())
                    })?;
                    search_world_bloom_rank_trace(
                        &engine,
                        event_id,
                        character_id,
                        rank,
                        &filter,
                        mode,
                    )
                    .await?
                }
                SubjectKind::User => {
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
            },
            None => match subject_kind {
                SubjectKind::Rank => {
                    let rank = resolved_rank.ok_or_else(|| {
                        ApiError::ServiceUnavailable("rank subject has no resolved rank".into())
                    })?;
                    search_rank_trace(&engine, event_id, rank, &filter, mode).await?
                }
                SubjectKind::User => {
                    search_user_trace(&engine, event_id, &user_id, &filter, mode).await?
                }
            },
        };
        if rank_data.is_empty() {
            return Err(ApiError::NotFound);
        }
        let user_data = get_user_data(&engine, event_id, &user_id, mode)
            .await
            .ok()
            .flatten();
        Ok(SubjectTraceResponseSchema {
            meta: meta(&server, event_id, character_id, Utc::now().timestamp()),
            subject: SubjectTraceMetaSchema {
                subject_type: subject_type.to_owned(),
                subject,
                resolved_user_id: Some(user_id),
                resolved_rank,
            },
            current,
            rank_data,
            user_data,
        })
    };
    cached_trace(&state, &cache_server, event_id, suffix, fetch).await
}

async fn fetch_snapshot_items(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    ranks: &[i64],
    mode: PublicUserIdMode,
    at: Option<i64>,
) -> Result<BTreeMap<i64, WebRankingItemSchema>, ApiError> {
    if ranks.is_empty() {
        return Ok(BTreeMap::new());
    }
    let max_rank = ranks.iter().copied().max().unwrap_or(0);
    let filter = WebRankingFilter {
        rank_min: Some(1),
        rank_max: Some(max_rank),
        score_min: None,
        score_max: None,
        start_time: None,
        end_time: None,
        before: None,
        after: None,
        timestamp: at,
        cursor: None,
        limit: max_rank as u64,
    };
    let wanted = ranks.iter().copied().collect::<BTreeSet<_>>();
    let mut out = BTreeMap::new();
    match character_id {
        Some(character_id) => {
            let (rows, _) =
                search_world_bloom_ranking_rows(engine, event_id, character_id, &filter, mode)
                    .await?;
            for row in rows {
                let item = row.into_web_item();
                if let Some(rank) = rank_of_item(&item)
                    && wanted.contains(&rank)
                {
                    out.insert(rank, item);
                }
            }
        }
        None => {
            let (rows, _) = search_ranking_rows(engine, event_id, &filter, mode).await?;
            for row in rows {
                let item = row.into_web_item();
                if let Some(rank) = rank_of_item(&item)
                    && wanted.contains(&rank)
                {
                    out.insert(rank, item);
                }
            }
        }
    }
    Ok(out)
}

async fn fetch_snapshot_metrics(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    ranks: &[i64],
    start_time: i64,
    end_time: Option<i64>,
) -> Result<BTreeMap<i64, RankingScoreGrowthSchema>, ApiError> {
    let growths = match character_id {
        Some(character_id) => {
            fetch_world_bloom_ranking_score_growths(
                engine,
                event_id,
                character_id,
                ranks,
                start_time,
                end_time,
            )
            .await?
        }
        None => fetch_ranking_score_growths(engine, event_id, ranks, start_time, end_time).await?,
    };
    Ok(growths
        .into_iter()
        .map(|growth| (growth.rank, growth))
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubjectKind {
    User,
    Rank,
}

async fn resolve_subject(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    subject: &str,
    subject_type: &str,
    mode: PublicUserIdMode,
    include_current: bool,
) -> Result<
    (
        String,
        Option<i64>,
        Option<WebRankingItemSchema>,
        SubjectKind,
    ),
    ApiError,
> {
    if subject_type.eq_ignore_ascii_case("rank") {
        let rank = subject
            .parse::<i64>()
            .map_err(|_| ApiError::BadRequest("rank subject must be an integer".into()))?;
        if rank <= 0 {
            return Err(ApiError::BadRequest("rank subject must be positive".into()));
        }
        let current = match character_id {
            Some(character_id) => {
                fetch_latest_world_bloom_ranking_by_rank(engine, event_id, rank, character_id, mode)
                    .await?
                    .map(RecordedRankData::WorldBloom)
            }
            None => fetch_latest_ranking_by_rank(engine, event_id, rank, mode)
                .await?
                .map(RecordedRankData::Normal),
        };
        let Some(rank_data) = current else {
            return Err(ApiError::NotFound);
        };
        let user_id = user_id_of_rank_data(&rank_data).ok_or_else(|| {
            ApiError::ServiceUnavailable("latest rank response has no user id".into())
        })?;
        let current_item = include_current.then_some(WebRankingItemSchema {
            rank_data,
            user_data: None,
        });
        return Ok((user_id, Some(rank), current_item, SubjectKind::Rank));
    }
    if !subject_type.eq_ignore_ascii_case("user") {
        return Err(ApiError::BadRequest(
            "subjectType must be user or rank".into(),
        ));
    }
    let current = if include_current {
        match character_id {
            Some(character_id) => {
                let latest = crate::db::query::world_bloom::fetch_latest_world_bloom_ranking(
                    engine,
                    event_id,
                    subject,
                    character_id,
                    mode,
                )
                .await?
                .map(RecordedRankData::WorldBloom);
                latest.map(|rank_data| WebRankingItemSchema {
                    rank_data,
                    user_data: None,
                })
            }
            None => fetch_latest_user_rank(engine, event_id, subject, mode).await?,
        }
    } else {
        None
    };
    let resolved_rank = current.as_ref().and_then(rank_of_item);
    Ok((
        subject.to_owned(),
        resolved_rank,
        current,
        SubjectKind::User,
    ))
}

async fn fetch_latest_user_rank(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
    mode: PublicUserIdMode,
) -> Result<Option<WebRankingItemSchema>, ApiError> {
    let rows = fetch_all_rankings(engine, event_id, user_id, mode).await?;
    Ok(rows.into_iter().last().map(|rank| WebRankingItemSchema {
        rank_data: RecordedRankData::Normal(rank),
        user_data: None,
    }))
}

fn user_id_of_rank_data(rank_data: &RecordedRankData) -> Option<String> {
    match rank_data {
        RecordedRankData::Normal(data) => Some(data.user_id.clone()),
        RecordedRankData::WorldBloom(data) => Some(data.user_id.clone()),
    }
}

async fn enrich_cloud_rank_infos_with_trace_metrics(
    state: &AppState,
    server: &str,
    event_id: i64,
    character_id: Option<i64>,
    ranks: &mut [CloudRankInfoSchema],
) {
    let Ok((region, engine)) = resolve_region_engine(state, server) else {
        return;
    };
    let Ok(mode) = prepare_user_id_mode(state, &engine, region, event_id).await else {
        return;
    };
    for rank in ranks {
        if has_cloud_round_metrics(rank) {
            continue;
        }
        let Some(user_id) = rank
            .user_id
            .as_deref()
            .filter(|user_id| !user_id.is_empty())
        else {
            continue;
        };
        let user_id = user_id.to_owned();
        let Ok(_permit) = state.query_limiter().acquire_trace(region).await else {
            continue;
        };
        let filter = WebTraceFilter {
            start_time: None,
            end_time: None,
            cursor: None,
            limit: CLOUD_TRACE_METRICS_LIMIT,
        };
        let trace = match character_id {
            Some(character_id) => {
                search_world_bloom_user_trace(
                    &engine,
                    event_id,
                    character_id,
                    user_id.as_str(),
                    &filter,
                    mode,
                )
                .await
            }
            None => search_user_trace(&engine, event_id, user_id.as_str(), &filter, mode).await,
        };
        let Ok(trace) = trace else {
            continue;
        };
        apply_cloud_trace_metrics_at(rank, &trace, Utc::now());
    }
}

fn has_cloud_round_metrics(info: &CloudRankInfoSchema) -> bool {
    info.average_round.is_some()
        || info.average_pt.is_some()
        || info.hour_round.is_some()
        || info.min20_times_3_speed.is_some()
        || info.record_start_at.is_some()
}

fn cloud_infos_from_snapshots(
    snapshots: &RankSnapshotsResponseSchema,
    skip_missing: bool,
) -> Result<Vec<CloudRankInfoSchema>, ApiError> {
    let mut out = Vec::with_capacity(snapshots.items.len());
    for item in &snapshots.items {
        if let Some(current) = item.current.as_ref()
            && let Some(info) = cloud_info_from_item_with_metrics(current, item.metrics.as_ref())
        {
            out.push(info);
            continue;
        }
        if !skip_missing {
            return Err(ApiError::NotFound);
        }
    }
    if out.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(out)
}

fn cloud_info_from_item(item: &WebRankingItemSchema) -> Option<CloudRankInfoSchema> {
    cloud_info_from_item_with_metrics(item, None)
}

fn cloud_info_from_item_with_metrics(
    item: &WebRankingItemSchema,
    metrics: Option<&RankingScoreGrowthSchema>,
) -> Option<CloudRankInfoSchema> {
    let name = item
        .user_data
        .as_ref()
        .map(|user| user.name.as_str())
        .unwrap_or_default();
    cloud_info_from_rank_data(&item.rank_data, Some(name), metrics)
}

fn cloud_info_from_rank_data(
    rank_data: &RecordedRankData,
    name: Option<&str>,
    metrics: Option<&RankingScoreGrowthSchema>,
) -> Option<CloudRankInfoSchema> {
    let (rank, user_id, score, timestamp, character_id) = match rank_data {
        RecordedRankData::Normal(data) => (
            data.rank,
            data.user_id.clone(),
            data.score,
            data.timestamp,
            None,
        ),
        RecordedRankData::WorldBloom(data) => (
            data.rank,
            data.user_id.clone(),
            data.score,
            data.timestamp,
            data.character_id,
        ),
    };
    if rank <= 0 {
        return None;
    }
    Some(CloudRankInfoSchema {
        rank,
        user_id: (!user_id.is_empty()).then_some(user_id),
        name: name.unwrap_or_default().to_owned(),
        score,
        timestamp,
        average_round: None,
        average_pt: None,
        latest_pt: metrics.and_then(|metrics| {
            metrics
                .score_earlier
                .map(|earlier| metrics.score_latest - earlier)
        }),
        speed: metrics.and_then(|metrics| metrics.growth),
        min20_times_3_speed: None,
        hour_round: None,
        record_start_at: None,
        speed_window: metrics.and_then(|metrics| metrics.time_diff),
        character_id,
    })
}

#[derive(Debug, Clone, Copy)]
struct CloudTraceSample {
    score: i64,
    timestamp: i64,
}

fn apply_cloud_trace_metrics_at(
    info: &mut CloudRankInfoSchema,
    trace: &[RecordedRankData],
    now: DateTime<Utc>,
) {
    let mut samples = trace
        .iter()
        .filter_map(cloud_trace_sample)
        .filter(|sample| sample.timestamp > 0)
        .collect::<Vec<_>>();
    if samples.is_empty() {
        return;
    }
    samples.sort_by_key(|sample| normalize_tracker_unix_seconds(sample.timestamp));

    info.record_start_at = Some(format_tracker_timestamp(samples[0].timestamp));
    if samples.len() < 2 {
        return;
    }

    let deltas = samples
        .windows(2)
        .filter_map(|window| {
            let diff = window[1].score - window[0].score;
            (diff > 0).then_some(diff)
        })
        .collect::<Vec<_>>();
    if let Some(latest) = deltas.last().copied() {
        info.latest_pt = Some(latest);
        let avg_window = if deltas.len() > 10 {
            &deltas[deltas.len() - 10..]
        } else {
            &deltas[..]
        };
        let round_count = avg_window.len() as i64;
        if round_count > 0 {
            let sum = avg_window.iter().sum::<i64>();
            info.average_round = Some(round_count);
            info.average_pt = Some(sum / round_count);
        }
    }

    let Some(last) = samples.last().copied() else {
        return;
    };
    let end_sec = effective_tracker_window_end_unix_seconds(last.timestamp, now);

    let hour_start = end_sec - 60 * 60;
    if let Some(hour_base_idx) = find_window_baseline_index(&samples, hour_start) {
        let hour_base = samples[hour_base_idx];
        let hour_base_sec = normalize_tracker_unix_seconds(hour_base.timestamp);
        if end_sec > hour_base_sec {
            let hour_gain = (last.score - hour_base.score).max(0);
            let hour_elapsed = end_sec - hour_base_sec;
            info.speed = Some(hour_gain * 3600 / hour_elapsed);
        }
        info.hour_round = Some(count_positive_deltas(&samples[hour_base_idx..]));
    }

    let window_start = end_sec - 20 * 60;
    if let Some(window_base_idx) = find_window_baseline_index(&samples, window_start) {
        let window_base = samples[window_base_idx];
        let window_gain = (last.score - window_base.score).max(0);
        info.min20_times_3_speed = Some(window_gain * 3);
    }
}

fn cloud_trace_sample(rank_data: &RecordedRankData) -> Option<CloudTraceSample> {
    match rank_data {
        RecordedRankData::Normal(data) => Some(CloudTraceSample {
            score: data.score,
            timestamp: data.timestamp,
        }),
        RecordedRankData::WorldBloom(data) => Some(CloudTraceSample {
            score: data.score,
            timestamp: data.timestamp,
        }),
    }
}

fn normalize_tracker_unix_seconds(timestamp: i64) -> i64 {
    if timestamp > 1_000_000_000_000 {
        timestamp / 1000
    } else {
        timestamp
    }
}

fn format_tracker_timestamp(timestamp: i64) -> i64 {
    if timestamp > 1_000_000_000_000 {
        timestamp
    } else {
        timestamp * 1000
    }
}

fn effective_tracker_window_end_unix_seconds(last_timestamp: i64, now: DateTime<Utc>) -> i64 {
    let last_sec = normalize_tracker_unix_seconds(last_timestamp);
    if last_sec <= 0 {
        return last_sec;
    }
    let now_sec = now.timestamp();
    if now_sec <= last_sec || now_sec - last_sec > TRACKER_REALTIME_TAIL_MAX_LAG_SECONDS {
        return last_sec;
    }
    now_sec
}

fn find_window_baseline_index(samples: &[CloudTraceSample], window_start: i64) -> Option<usize> {
    if samples.is_empty() {
        return None;
    }
    let mut baseline = None;
    for (idx, sample) in samples.iter().enumerate() {
        let sec = normalize_tracker_unix_seconds(sample.timestamp);
        if sec <= window_start {
            baseline = Some(idx);
            continue;
        }
        break;
    }
    Some(baseline.unwrap_or(0))
}

fn count_positive_deltas(samples: &[CloudTraceSample]) -> i64 {
    samples
        .windows(2)
        .filter(|window| window[1].score - window[0].score > 0)
        .count() as i64
}

fn rank_of_item(item: &WebRankingItemSchema) -> Option<i64> {
    match &item.rank_data {
        RecordedRankData::Normal(data) => Some(data.rank),
        RecordedRankData::WorldBloom(data) => Some(data.rank),
    }
}

async fn cached_snapshot<T, Fut>(
    state: &AppState,
    server: &str,
    event_id: i64,
    suffix: String,
    fetch: Fut,
) -> Result<T, ApiError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                server,
                event_id,
                suffix,
                cache.ttl(CacheTtl::LatestRank),
                fetch,
            )
            .await
    } else {
        fetch.await
    }
}

fn meta(
    server: &str,
    event_id: i64,
    character_id: Option<i64>,
    fetched_at: i64,
) -> LeaderboardMetaSchema {
    LeaderboardMetaSchema {
        server: server.to_owned(),
        event_id,
        scope: match character_id {
            Some(character_id) => format!("world-bloom/{character_id}"),
            None => "total".to_owned(),
        },
        character_id,
        fetched_at,
    }
}

fn interval_seconds(interval: Option<i64>) -> i64 {
    interval
        .unwrap_or(DEFAULT_INTERVAL_SECONDS)
        .clamp(1, 86_400)
}

fn positive_timestamp(timestamp: Option<i64>) -> Option<i64> {
    timestamp.filter(|timestamp| *timestamp > 0)
}

fn join_ranks(ranks: &[i64]) -> String {
    ranks
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sonic_rs::JsonValueTrait;

    #[test]
    fn interval_defaults_and_clamps() {
        assert_eq!(interval_seconds(None), DEFAULT_INTERVAL_SECONDS);
        assert_eq!(interval_seconds(Some(0)), 1);
        assert_eq!(interval_seconds(Some(90_000)), 86_400);
    }

    #[test]
    fn meta_uses_stable_scope_names() {
        assert_eq!(meta("cn", 170, None, 1).scope, "total");
        assert_eq!(meta("cn", 170, Some(19), 1).scope, "world-bloom/19");
    }

    #[test]
    fn join_ranks_preserves_request_order() {
        assert_eq!(join_ranks(&[10, 1, 100]), "10,1,100");
    }

    #[test]
    fn cloud_rank_info_serializes_round_metric_fields() {
        let info = CloudRankInfoSchema {
            rank: 100,
            user_id: Some("12345".to_owned()),
            name: "User".to_owned(),
            score: 1_500_000,
            timestamp: 1_704_067_200,
            average_round: Some(2),
            average_pt: Some(250_000),
            latest_pt: Some(300_000),
            speed: Some(600_000),
            min20_times_3_speed: Some(900_000),
            hour_round: Some(2),
            record_start_at: Some(1_704_060_000_000),
            speed_window: Some(3_600),
            character_id: Some(25),
        };

        let value = sonic_rs::to_value(&info).expect("serialize rank info");
        assert_eq!(value["averageRound"].as_i64(), Some(2));
        assert_eq!(value["averagePt"].as_i64(), Some(250_000));
        assert_eq!(value["latestPt"].as_i64(), Some(300_000));
        assert_eq!(value["min20Times3Speed"].as_i64(), Some(900_000));
        assert_eq!(value["hourRound"].as_i64(), Some(2));
        assert_eq!(value["recordStartAt"].as_i64(), Some(1_704_060_000_000));
    }

    #[test]
    fn cloud_trace_metrics_match_cloud_fallback_semantics() {
        let trace = vec![
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_000_000,
                timestamp: 1_704_060_000,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_250_000,
                timestamp: 1_704_063_600,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_550_000,
                timestamp: 1_704_067_200,
            }),
        ];
        let mut info = CloudRankInfoSchema {
            rank: 100,
            user_id: Some("12345".to_owned()),
            name: "User".to_owned(),
            score: 1_550_000,
            timestamp: 1_704_067_200,
            average_round: None,
            average_pt: None,
            latest_pt: None,
            speed: None,
            min20_times_3_speed: None,
            hour_round: None,
            record_start_at: None,
            speed_window: None,
            character_id: None,
        };

        apply_cloud_trace_metrics_at(
            &mut info,
            &trace,
            Utc.timestamp_opt(1_704_067_200, 0).single().unwrap(),
        );

        assert_eq!(info.record_start_at, Some(1_704_060_000_000));
        assert_eq!(info.latest_pt, Some(300_000));
        assert_eq!(info.average_round, Some(2));
        assert_eq!(info.average_pt, Some(275_000));
        assert_eq!(info.speed, Some(300_000));
        assert_eq!(info.hour_round, Some(1));
        assert_eq!(info.min20_times_3_speed, Some(900_000));
    }

    #[test]
    fn tracker_window_end_ignores_stale_tail() {
        let last_timestamp = 1_704_067_200;
        let now = Utc
            .timestamp_opt(
                last_timestamp + TRACKER_REALTIME_TAIL_MAX_LAG_SECONDS + 1,
                0,
            )
            .single()
            .unwrap();

        assert_eq!(
            effective_tracker_window_end_unix_seconds(last_timestamp, now),
            last_timestamp
        );
    }
}
