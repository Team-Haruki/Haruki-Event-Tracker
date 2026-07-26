use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;

use crate::api::error::ApiError;
use crate::api::handler::leaderboard::service::{
    OverviewQuery, WebDetailQuery, web_overview_for_scope, web_rank_detail_for_scope,
    web_user_detail_for_scope,
};
use crate::api::handler::web::UserSearchQuery;
use crate::api::json::{EncodedJson, Json, RawJson, accepts_gzip};
use crate::api::state::AppState;
use crate::model::api::{WebRankDetailResponseSchema, WebUserDetailResponseSchema};

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn total_overview(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(state, server, event_id, None, query, "web:v2", prefer_gzip).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_overview(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        "web:v2",
        prefer_gzip,
    )
    .await
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn total_replay_overview(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(
        state,
        server,
        event_id,
        None,
        query,
        "web:v2:replay",
        prefer_gzip,
    )
    .await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_replay_overview(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        "web:v2:replay",
        prefer_gzip,
    )
    .await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, rank))]
pub async fn total_rank_detail(
    State(state): State<AppState>,
    Path((server, event_id, rank)): Path<(String, i64, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebRankDetailResponseSchema>, ApiError> {
    web_rank_detail_for_scope(state, server, event_id, None, rank, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id, rank))]
pub async fn world_bloom_rank_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, rank)): Path<(String, i64, i64, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebRankDetailResponseSchema>, ApiError> {
    web_rank_detail_for_scope(state, server, event_id, Some(character_id), rank, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, user_id))]
pub async fn total_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(state, server, event_id, None, user_id, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id, user_id))]
pub async fn world_bloom_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(state, server, event_id, Some(character_id), user_id, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_users(
    State(state): State<AppState>,
    Path((server, event_id, _character_id)): Path<(String, i64, i64)>,
    Query(query): Query<UserSearchQuery>,
) -> Result<RawJson, ApiError> {
    crate::api::handler::web::users(State(state), Path((server, event_id)), Query(query)).await
}
