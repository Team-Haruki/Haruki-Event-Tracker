//! `latest-world-bloom-ranking/{user,rank}` — World Bloom per-chapter
//! variants of `handler::ranking`. Same 404 rules as the normal flow.
//! Mirrors `getWorldBloomRankingByUserID` / `getWorldBloomRankingByRank`
//! in `api/api.go`.

use axum::extract::{Path, State};

use crate::api::cache::{CacheTtl, user_suffix, wb_rank_suffix};
use crate::api::error::ApiError;
use crate::api::extract::{prepare_user_id_mode, resolve_region_engine};
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::query::user::get_user_data;
use crate::db::query::world_bloom::{
    fetch_latest_world_bloom_ranking, fetch_latest_world_bloom_ranking_by_rank,
};
use crate::model::api::{RecordedRankData, UserLatestRankingQueryResponseSchema};

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn latest_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let ranking =
            fetch_latest_world_bloom_ranking(&engine, event_id, &user_id, character_id, mode)
                .await?;
        let user_data = get_user_data(&engine, event_id, &user_id, mode)
            .await
            .ok()
            .flatten();
        if ranking.is_none() && user_data.is_none() {
            return Err(ApiError::NotFound);
        }
        Ok(UserLatestRankingQueryResponseSchema {
            rank_data: ranking.map(RecordedRankData::WorldBloom),
            user_data,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                format!("wb:{character_id}:{}", user_suffix("latest", &user_id)),
                cache.ttl(CacheTtl::LatestRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, rank))]
pub async fn latest_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, character_id, rank)): Path<(String, i64, i64, i64)>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let ranking =
            fetch_latest_world_bloom_ranking_by_rank(&engine, event_id, rank, character_id, mode)
                .await?;
        let Some(ranking) = ranking else {
            return Err(ApiError::NotFound);
        };
        let user_data = get_user_data(&engine, event_id, &ranking.user_id, mode)
            .await
            .ok()
            .flatten();
        Ok(UserLatestRankingQueryResponseSchema {
            rank_data: Some(RecordedRankData::WorldBloom(ranking)),
            user_data,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                wb_rank_suffix("latest", character_id, rank),
                cache.ttl(CacheTtl::LatestRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}
