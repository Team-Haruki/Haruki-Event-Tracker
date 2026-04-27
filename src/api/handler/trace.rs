//! `trace-ranking` — full history endpoints. Returns every recorded
//! row for a (user|rank), normal or World Bloom. Mirrors the four
//! `getAll*` handlers in `api/api.go`. 404 rules are the same as the
//! `latest-*` siblings.

use axum::extract::{Path, State};

use crate::api::error::ApiError;
use crate::api::extract::resolve_engine;
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::query::ranking::{fetch_all_rankings, fetch_all_rankings_by_rank};
use crate::db::query::user::get_user_data;
use crate::db::query::world_bloom::{
    fetch_all_world_bloom_rankings, fetch_all_world_bloom_rankings_by_rank,
};
use crate::model::api::{RecordedRankData, UserAllRankingDataQueryResponseSchema};

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn all_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let rankings = fetch_all_rankings(&engine, event_id, &user_id).await?;
    let user_data = get_user_data(&engine, event_id, &user_id).await.ok().flatten();
    if rankings.is_empty() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings.into_iter().map(RecordedRankData::Normal).collect(),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, rank))]
pub async fn all_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, rank)): Path<(String, i64, i64)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let rankings = fetch_all_rankings_by_rank(&engine, event_id, rank).await?;
    if rankings.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings.into_iter().map(RecordedRankData::Normal).collect(),
        user_data: None,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn wb_all_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let rankings =
        fetch_all_world_bloom_rankings(&engine, event_id, &user_id, character_id).await?;
    let user_data = get_user_data(&engine, event_id, &user_id).await.ok().flatten();
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

#[tracing::instrument(skip(state), fields(server, event_id, character_id, rank))]
pub async fn wb_all_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, character_id, rank)): Path<(String, i64, i64, i64)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let rankings =
        fetch_all_world_bloom_rankings_by_rank(&engine, event_id, rank, character_id).await?;
    if rankings.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings
            .into_iter()
            .map(RecordedRankData::WorldBloom)
            .collect(),
        user_data: None,
    }))
}
