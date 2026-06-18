use axum::extract::{Path, Query, State};

use crate::api::error::ApiError;
use crate::api::handler::leaderboard::service::{
    CloudQuery, cloud_check_room_for_scope, cloud_line_for_scope, cloud_query_for_scope,
    cloud_speed_for_scope, cloud_trace_for_scope,
};
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::model::api::{
    CloudCheckRoomResponseSchema, CloudLineResponseSchema, CloudRankQueryResponseSchema,
    CloudSpeedResponseSchema, CloudTraceResponseSchema,
};

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id))]
pub async fn total_query(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudRankQueryResponseSchema>, ApiError> {
    cloud_query_for_scope(state, server, event_id, None, query, raw_query, true).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn world_bloom_query(
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
pub async fn total_check_room(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudCheckRoomResponseSchema>, ApiError> {
    cloud_check_room_for_scope(state, server, event_id, None, query, raw_query).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn world_bloom_check_room(
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
pub async fn total_line(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudLineResponseSchema>, ApiError> {
    cloud_line_for_scope(state, server, event_id, None, query, raw_query).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn world_bloom_line(
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
pub async fn total_speed(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<CloudSpeedResponseSchema>, ApiError> {
    cloud_speed_for_scope(state, server, event_id, None, query, raw_query).await
}

#[tracing::instrument(skip(state, query, raw_query), fields(server, event_id, character_id))]
pub async fn world_bloom_speed(
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
pub async fn total_trace(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<CloudQuery>,
) -> Result<Json<CloudTraceResponseSchema>, ApiError> {
    cloud_trace_for_scope(state, server, event_id, None, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_trace(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<CloudQuery>,
) -> Result<Json<CloudTraceResponseSchema>, ApiError> {
    cloud_trace_for_scope(state, server, event_id, Some(character_id), query).await
}
