//! `latest-ranking/{user,rank}` — main event ranking lookups. Mirrors
//! Go `getNormalRankingByUserID` / `getNormalRankingByRank` in
//! `api/api.go`.
//!
//! 404 semantics match the Go reference: "user lookup" returns 404 only
//! if both ranking and user-data come back empty (lets clients see a
//! user's name even after their score was overwritten); "rank lookup"
//! returns 404 if no ranking row exists.

use axum::extract::{Path, State};

use crate::api::cache::{CacheTtl, rank_suffix, user_suffix};
use crate::api::error::ApiError;
use crate::api::extract::{prepare_user_id_mode, resolve_region_engine};
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::query::ranking::{fetch_latest_ranking, fetch_latest_ranking_by_rank};
use crate::db::query::user::get_user_data;
use crate::model::api::{RecordedRankData, UserLatestRankingQueryResponseSchema};

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn latest_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let ranking = fetch_latest_ranking(&engine, event_id, &user_id, mode).await?;
        let user_data = get_user_data(&engine, event_id, &user_id, mode)
            .await
            .ok()
            .flatten();
        if ranking.is_none() && user_data.is_none() {
            return Err(ApiError::NotFound);
        }
        Ok(UserLatestRankingQueryResponseSchema {
            rank_data: ranking.map(RecordedRankData::Normal),
            user_data,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                user_suffix("latest", &user_id),
                cache.ttl(CacheTtl::LatestRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}

#[tracing::instrument(skip(state), fields(server, event_id, rank))]
pub async fn latest_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, rank)): Path<(String, i64, i64)>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let ranking = fetch_latest_ranking_by_rank(&engine, event_id, rank, mode).await?;
        let Some(ranking) = ranking else {
            return Err(ApiError::NotFound);
        };
        let user_data = get_user_data(&engine, event_id, &ranking.user_id, mode)
            .await
            .ok()
            .flatten();
        Ok(UserLatestRankingQueryResponseSchema {
            rank_data: Some(RecordedRankData::Normal(ranking)),
            user_data,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                rank_suffix("latest", rank),
                cache.ttl(CacheTtl::LatestRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}
