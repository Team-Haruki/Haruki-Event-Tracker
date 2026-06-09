//! `trace-ranking` — full history endpoints. Returns every recorded
//! row for a (user|rank), normal or World Bloom. Mirrors the four
//! `getAll*` handlers in `api/api.go`. 404 rules are the same as the
//! `latest-*` siblings.

use std::collections::HashMap;

use axum::extract::{Path, RawQuery, State};

use crate::api::cache::{
    CacheTtl, batch_rank_suffix, rank_suffix, user_suffix, wb_batch_rank_suffix, wb_rank_suffix,
};
use crate::api::error::ApiError;
use crate::api::extract::{parse_rank_query, prepare_user_id_mode, resolve_region_engine};
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::query::ranking::{
    fetch_all_rankings, fetch_all_rankings_by_rank, fetch_all_rankings_by_ranks,
};
use crate::db::query::user::get_user_data;
use crate::db::query::world_bloom::{
    fetch_all_world_bloom_rankings, fetch_all_world_bloom_rankings_by_rank,
    fetch_all_world_bloom_rankings_by_ranks,
};
use crate::model::api::{
    BatchAllRankingDataItemSchema, BatchAllRankingDataQueryResponseSchema, RecordedRankData,
    UserAllRankingDataQueryResponseSchema,
};

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn all_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let rankings = fetch_all_rankings(&engine, event_id, &user_id, mode).await?;
        let user_data = get_user_data(&engine, event_id, &user_id, mode)
            .await
            .ok()
            .flatten();
        if rankings.is_empty() && user_data.is_none() {
            return Err(ApiError::NotFound);
        }
        Ok(UserAllRankingDataQueryResponseSchema {
            rank_data: rankings.into_iter().map(RecordedRankData::Normal).collect(),
            user_data,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                user_suffix("trace", &user_id),
                cache.ttl(CacheTtl::TraceRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}

#[tracing::instrument(skip(state), fields(server, event_id, rank))]
pub async fn all_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, rank)): Path<(String, i64, i64)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let rankings = fetch_all_rankings_by_rank(&engine, event_id, rank, mode).await?;
        if rankings.is_empty() {
            return Err(ApiError::NotFound);
        }
        Ok(UserAllRankingDataQueryResponseSchema {
            rank_data: rankings.into_iter().map(RecordedRankData::Normal).collect(),
            user_data: None,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                rank_suffix("trace", rank),
                cache.ttl(CacheTtl::TraceRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}

#[tracing::instrument(skip(state, raw_query), fields(server, event_id))]
pub async fn all_by_ranks(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<BatchAllRankingDataQueryResponseSchema>, ApiError> {
    let ranks = parse_rank_query(raw_query.as_deref())?;
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let suffix = batch_rank_suffix("trace", &ranks);
    let fetch_ranks = ranks.clone();
    let fetch = async {
        let rankings = fetch_all_rankings_by_ranks(&engine, event_id, &fetch_ranks, mode).await?;
        if rankings.is_empty() {
            return Err(ApiError::NotFound);
        }

        let mut by_rank: HashMap<i64, Vec<RecordedRankData>> = HashMap::new();
        for ranking in rankings {
            by_rank
                .entry(ranking.rank)
                .or_default()
                .push(RecordedRankData::Normal(ranking));
        }
        let items = fetch_ranks
            .into_iter()
            .filter_map(|rank| {
                by_rank
                    .remove(&rank)
                    .map(|rank_data| BatchAllRankingDataItemSchema { rank, rank_data })
            })
            .collect();

        Ok(BatchAllRankingDataQueryResponseSchema { items })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                suffix,
                cache.ttl(CacheTtl::BatchTraceRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn wb_all_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let rankings =
            fetch_all_world_bloom_rankings(&engine, event_id, &user_id, character_id, mode).await?;
        let user_data = get_user_data(&engine, event_id, &user_id, mode)
            .await
            .ok()
            .flatten();
        if rankings.is_empty() && user_data.is_none() {
            return Err(ApiError::NotFound);
        }
        Ok(UserAllRankingDataQueryResponseSchema {
            rank_data: rankings
                .into_iter()
                .map(RecordedRankData::WorldBloom)
                .collect(),
            user_data,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                format!("wb:{character_id}:{}", user_suffix("trace", &user_id)),
                cache.ttl(CacheTtl::TraceRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}

#[tracing::instrument(skip(state, raw_query), fields(server, event_id, character_id))]
pub async fn wb_all_by_ranks(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<BatchAllRankingDataQueryResponseSchema>, ApiError> {
    let ranks = parse_rank_query(raw_query.as_deref())?;
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let suffix = wb_batch_rank_suffix("trace", character_id, &ranks);
    let fetch_ranks = ranks.clone();
    let fetch = async {
        let rankings = fetch_all_world_bloom_rankings_by_ranks(
            &engine,
            event_id,
            &fetch_ranks,
            character_id,
            mode,
        )
        .await?;
        if rankings.is_empty() {
            return Err(ApiError::NotFound);
        }

        let mut by_rank: HashMap<i64, Vec<RecordedRankData>> = HashMap::new();
        for ranking in rankings {
            by_rank
                .entry(ranking.rank)
                .or_default()
                .push(RecordedRankData::WorldBloom(ranking));
        }
        let items = fetch_ranks
            .into_iter()
            .filter_map(|rank| {
                by_rank
                    .remove(&rank)
                    .map(|rank_data| BatchAllRankingDataItemSchema { rank, rank_data })
            })
            .collect();

        Ok(BatchAllRankingDataQueryResponseSchema { items })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                suffix,
                cache.ttl(CacheTtl::BatchTraceRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, rank))]
pub async fn wb_all_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, character_id, rank)): Path<(String, i64, i64, i64)>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch = async {
        let rankings =
            fetch_all_world_bloom_rankings_by_rank(&engine, event_id, rank, character_id, mode)
                .await?;
        if rankings.is_empty() {
            return Err(ApiError::NotFound);
        }
        Ok(UserAllRankingDataQueryResponseSchema {
            rank_data: rankings
                .into_iter()
                .map(RecordedRankData::WorldBloom)
                .collect(),
            user_data: None,
        })
    };
    let response = if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                &server,
                event_id,
                wb_rank_suffix("trace", character_id, rank),
                cache.ttl(CacheTtl::TraceRank),
                fetch,
            )
            .await?
    } else {
        fetch.await?
    };
    Ok(Json(response))
}
