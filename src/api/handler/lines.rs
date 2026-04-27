//! `ranking-lines` and `ranking-score-growth` endpoints (normal + WB).
//! Mirrors `getRankingLines`, `getRankingScoreGrowths`,
//! `getWorldBloomRankingLines`, `getWorldBloomRankingScoreGrowths` in
//! `api/api.go`. The fixed rank-line lists live in `model::enums`.

use axum::extract::{Path, State};
use chrono::Utc;

use crate::api::error::ApiError;
use crate::api::extract::resolve_engine;
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::query::growth::{
    fetch_ranking_score_growths, fetch_world_bloom_ranking_score_growths,
};
use crate::db::query::lines::{fetch_ranking_lines, fetch_world_bloom_ranking_lines};
use crate::model::api::{RankingLineScoreSchema, RankingScoreGrowthSchema};
use crate::model::enums::{
    SEKAI_EVENT_RANKING_LINES_NORMAL, SEKAI_EVENT_RANKING_LINES_WORLD_BLOOM,
};

#[tracing::instrument(skip(state), fields(server, event_id))]
pub async fn ranking_lines(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
) -> Result<Json<Vec<RankingLineScoreSchema>>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let rows = fetch_ranking_lines(&engine, event_id, SEKAI_EVENT_RANKING_LINES_NORMAL).await?;
    Ok(Json(rows))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id))]
pub async fn wb_ranking_lines(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
) -> Result<Json<Vec<RankingLineScoreSchema>>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let rows = fetch_world_bloom_ranking_lines(
        &engine,
        event_id,
        character_id,
        SEKAI_EVENT_RANKING_LINES_WORLD_BLOOM,
    )
    .await?;
    Ok(Json(rows))
}

#[tracing::instrument(skip(state), fields(server, event_id, interval))]
pub async fn score_growth(
    State(state): State<AppState>,
    Path((server, event_id, interval)): Path<(String, i64, i64)>,
) -> Result<Json<Vec<RankingScoreGrowthSchema>>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let start_time = Utc::now().timestamp() - interval;
    let rows = fetch_ranking_score_growths(
        &engine,
        event_id,
        SEKAI_EVENT_RANKING_LINES_NORMAL,
        start_time,
    )
    .await?;
    Ok(Json(rows))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, interval))]
pub async fn wb_score_growth(
    State(state): State<AppState>,
    Path((server, event_id, character_id, interval)): Path<(String, i64, i64, i64)>,
) -> Result<Json<Vec<RankingScoreGrowthSchema>>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let start_time = Utc::now().timestamp() - interval;
    let rows = fetch_world_bloom_ranking_score_growths(
        &engine,
        event_id,
        character_id,
        SEKAI_EVENT_RANKING_LINES_WORLD_BLOOM,
        start_time,
    )
    .await?;
    Ok(Json(rows))
}
