//! `trace-ranking` — full history endpoints. Returns every recorded
//! row for a (user|rank), normal or World Bloom. Mirrors the four
//! `getAll*` handlers in `api/api.go`. 404 rules are the same as the
//! `latest-*` siblings.

use std::collections::HashMap;

use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::http::header::ACCEPT_ENCODING;
use bytes::Bytes;
use futures::stream::{self, StreamExt};

use crate::api::cache::{
    CacheTtl, CachedJsonEncoding, batch_rank_suffix, rank_suffix, user_suffix,
    wb_batch_rank_suffix, wb_rank_suffix,
};
use crate::api::error::ApiError;
use crate::api::extract::{parse_rank_query, prepare_user_id_mode, resolve_region_engine};
use crate::api::json::{EncodedJson, RawJson};
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
use crate::model::enums::SekaiServerRegion;

async fn fetch_rank_trace_response(
    engine: &crate::db::engine::DatabaseEngine,
    event_id: i64,
    rank: i64,
    mode: crate::db::query::user::PublicUserIdMode,
) -> Result<UserAllRankingDataQueryResponseSchema, ApiError> {
    let rankings = fetch_all_rankings_by_rank(engine, event_id, rank, mode).await?;
    if rankings.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings.into_iter().map(RecordedRankData::Normal).collect(),
        user_data: None,
    })
}

async fn fetch_wb_rank_trace_response(
    engine: &crate::db::engine::DatabaseEngine,
    event_id: i64,
    rank: i64,
    character_id: i64,
    mode: crate::db::query::user::PublicUserIdMode,
) -> Result<UserAllRankingDataQueryResponseSchema, ApiError> {
    let rankings =
        fetch_all_world_bloom_rankings_by_rank(engine, event_id, rank, character_id, mode).await?;
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
}

fn batch_item_from_trace(
    rank: i64,
    response: UserAllRankingDataQueryResponseSchema,
) -> BatchAllRankingDataItemSchema {
    BatchAllRankingDataItemSchema {
        rank,
        rank_data: response.rank_data,
    }
}

fn batch_response_from_results(
    results: Vec<Result<Option<BatchAllRankingDataItemSchema>, ApiError>>,
) -> Result<BatchAllRankingDataQueryResponseSchema, ApiError> {
    let mut items = Vec::new();
    for result in results {
        if let Some(item) = result? {
            items.push(item);
        }
    }
    if items.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(BatchAllRankingDataQueryResponseSchema { items })
}

fn raw_json<T: serde::Serialize>(value: &T) -> Result<RawJson, ApiError> {
    sonic_rs::to_vec(value)
        .map(|bytes| RawJson(Bytes::from(bytes)))
        .map_err(|err| {
            tracing::error!(?err, "json encode error");
            ApiError::ServiceUnavailable("json encode error".into())
        })
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .filter_map(|part| part.split(';').next())
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("gzip"))
        })
}

fn encoded_json(bytes: crate::api::cache::CachedJson) -> EncodedJson {
    match bytes.encoding {
        CachedJsonEncoding::Identity => EncodedJson::identity(bytes.bytes),
        CachedJsonEncoding::Gzip => EncodedJson::gzip(bytes.bytes),
    }
}

struct BatchTraceContext<'a> {
    cache: &'a crate::api::cache::ApiCache,
    state: &'a AppState,
    server: &'a str,
    event_id: i64,
    region: SekaiServerRegion,
    engine: &'a crate::db::engine::DatabaseEngine,
    mode: crate::db::query::user::PublicUserIdMode,
}

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn all_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let limiter = state.query_limiter().clone();
    let suffix = user_suffix("trace", &user_id);
    let fetch = async move {
        let _permit = limiter.acquire_trace(region).await?;
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
    if let Some(cache) = state.cache() {
        let bytes = cache
            .get_or_fetch_encoded_json(
                &server,
                event_id,
                suffix,
                cache.ttl(CacheTtl::TraceRank),
                accepts_gzip(&headers),
                fetch,
            )
            .await?;
        Ok(encoded_json(bytes))
    } else {
        raw_json(&fetch.await?).map(|json| EncodedJson::identity(json.0))
    }
}

#[tracing::instrument(skip(state), fields(server, event_id, rank))]
pub async fn all_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, rank)): Path<(String, i64, i64)>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let limiter = state.query_limiter().clone();
    let fetch = async move {
        let _permit = limiter.acquire_trace(region).await?;
        fetch_rank_trace_response(&engine, event_id, rank, mode).await
    };
    if let Some(cache) = state.cache() {
        let bytes = cache
            .get_or_fetch_encoded_json(
                &server,
                event_id,
                rank_suffix("trace", rank),
                cache.ttl(CacheTtl::TraceRank),
                accepts_gzip(&headers),
                fetch,
            )
            .await?;
        Ok(encoded_json(bytes))
    } else {
        raw_json(&fetch.await?).map(|json| EncodedJson::identity(json.0))
    }
}

#[tracing::instrument(skip(state, raw_query), fields(server, event_id))]
pub async fn all_by_ranks(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let ranks = parse_rank_query(raw_query.as_deref())?;
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch_response = async {
        if let Some(cache) = state.cache() {
            fetch_batch_by_ranks_from_single_rank_cache(
                BatchTraceContext {
                    cache,
                    state: &state,
                    server: &server,
                    event_id,
                    region,
                    engine: &engine,
                    mode,
                },
                &ranks,
            )
            .await
        } else {
            fetch_batch_by_ranks_sql(&state, region, &engine, event_id, ranks.clone(), mode).await
        }
    };

    if let Some(cache) = state.cache() {
        let fetch = async {
            let response = fetch_response.await?;
            sonic_rs::to_vec(&response).map(Bytes::from).map_err(|err| {
                tracing::error!(?err, "json encode error");
                ApiError::ServiceUnavailable("json encode error".into())
            })
        };
        let bytes = cache
            .get_or_fetch_batch_encoded_json(
                &server,
                event_id,
                batch_rank_suffix("trace", &ranks),
                cache.ttl(CacheTtl::BatchTraceRank),
                accepts_gzip(&headers),
                fetch,
            )
            .await?;
        Ok(encoded_json(bytes))
    } else {
        raw_json(&fetch_response.await?).map(|json| EncodedJson::identity(json.0))
    }
}

async fn fetch_batch_by_ranks_from_single_rank_cache(
    ctx: BatchTraceContext<'_>,
    ranks: &[i64],
) -> Result<BatchAllRankingDataQueryResponseSchema, ApiError> {
    let cache = ctx.cache.clone();
    let limiter = ctx.state.query_limiter().clone();
    let concurrency = limiter.batch_trace_fill_concurrency();
    let results = stream::iter(ranks.iter().copied().map(|rank| {
        let cache = cache.clone();
        let limiter = limiter.clone();
        async move {
            let fetch = async move {
                let _permit = limiter.acquire_trace(ctx.region).await?;
                fetch_rank_trace_response(ctx.engine, ctx.event_id, rank, ctx.mode).await
            };
            match cache
                .get_or_fetch(
                    ctx.server,
                    ctx.event_id,
                    rank_suffix("trace", rank),
                    cache.ttl(CacheTtl::TraceRank),
                    fetch,
                )
                .await
            {
                Ok(response) => Ok(Some(batch_item_from_trace(rank, response))),
                Err(ApiError::NotFound) => Ok(None),
                Err(err) => Err(err),
            }
        }
    }))
    .buffered(concurrency)
    .collect::<Vec<_>>()
    .await;

    batch_response_from_results(results)
}

async fn fetch_batch_by_ranks_sql(
    state: &AppState,
    region: crate::model::enums::SekaiServerRegion,
    engine: &crate::db::engine::DatabaseEngine,
    event_id: i64,
    ranks: Vec<i64>,
    mode: crate::db::query::user::PublicUserIdMode,
) -> Result<BatchAllRankingDataQueryResponseSchema, ApiError> {
    let limiter = state.query_limiter().clone();
    let _permit = limiter.acquire_trace(region).await?;
    let suffix = batch_rank_suffix("trace", &ranks);
    tracing::debug!(%suffix, "api cache disabled for batch trace");
    let rankings = fetch_all_rankings_by_ranks(engine, event_id, &ranks, mode).await?;
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
    let items = ranks
        .into_iter()
        .filter_map(|rank| {
            by_rank
                .remove(&rank)
                .map(|rank_data| BatchAllRankingDataItemSchema { rank, rank_data })
        })
        .collect();

    Ok(BatchAllRankingDataQueryResponseSchema { items })
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn wb_all_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let limiter = state.query_limiter().clone();
    let suffix = format!("wb:{character_id}:{}", user_suffix("trace", &user_id));
    let fetch = async move {
        let _permit = limiter.acquire_trace(region).await?;
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
    if let Some(cache) = state.cache() {
        let bytes = cache
            .get_or_fetch_encoded_json(
                &server,
                event_id,
                suffix,
                cache.ttl(CacheTtl::TraceRank),
                accepts_gzip(&headers),
                fetch,
            )
            .await?;
        Ok(encoded_json(bytes))
    } else {
        raw_json(&fetch.await?).map(|json| EncodedJson::identity(json.0))
    }
}

#[tracing::instrument(skip(state, raw_query), fields(server, event_id, character_id))]
pub async fn wb_all_by_ranks(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let ranks = parse_rank_query(raw_query.as_deref())?;
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let fetch_response = async {
        if let Some(cache) = state.cache() {
            fetch_wb_batch_by_ranks_from_single_rank_cache(
                BatchTraceContext {
                    cache,
                    state: &state,
                    server: &server,
                    event_id,
                    region,
                    engine: &engine,
                    mode,
                },
                character_id,
                &ranks,
            )
            .await
        } else {
            fetch_wb_batch_by_ranks_sql(
                &state,
                region,
                &engine,
                event_id,
                character_id,
                ranks.clone(),
                mode,
            )
            .await
        }
    };

    if let Some(cache) = state.cache() {
        let fetch = async {
            let response = fetch_response.await?;
            sonic_rs::to_vec(&response).map(Bytes::from).map_err(|err| {
                tracing::error!(?err, "json encode error");
                ApiError::ServiceUnavailable("json encode error".into())
            })
        };
        let bytes = cache
            .get_or_fetch_batch_encoded_json(
                &server,
                event_id,
                wb_batch_rank_suffix("trace", character_id, &ranks),
                cache.ttl(CacheTtl::BatchTraceRank),
                accepts_gzip(&headers),
                fetch,
            )
            .await?;
        Ok(encoded_json(bytes))
    } else {
        raw_json(&fetch_response.await?).map(|json| EncodedJson::identity(json.0))
    }
}

async fn fetch_wb_batch_by_ranks_from_single_rank_cache(
    ctx: BatchTraceContext<'_>,
    character_id: i64,
    ranks: &[i64],
) -> Result<BatchAllRankingDataQueryResponseSchema, ApiError> {
    let cache = ctx.cache.clone();
    let limiter = ctx.state.query_limiter().clone();
    let concurrency = limiter.batch_trace_fill_concurrency();
    let results = stream::iter(ranks.iter().copied().map(|rank| {
        let cache = cache.clone();
        let limiter = limiter.clone();
        async move {
            let fetch = async move {
                let _permit = limiter.acquire_trace(ctx.region).await?;
                fetch_wb_rank_trace_response(ctx.engine, ctx.event_id, rank, character_id, ctx.mode)
                    .await
            };
            match cache
                .get_or_fetch(
                    ctx.server,
                    ctx.event_id,
                    wb_rank_suffix("trace", character_id, rank),
                    cache.ttl(CacheTtl::TraceRank),
                    fetch,
                )
                .await
            {
                Ok(response) => Ok(Some(batch_item_from_trace(rank, response))),
                Err(ApiError::NotFound) => Ok(None),
                Err(err) => Err(err),
            }
        }
    }))
    .buffered(concurrency)
    .collect::<Vec<_>>()
    .await;

    batch_response_from_results(results)
}

async fn fetch_wb_batch_by_ranks_sql(
    state: &AppState,
    region: crate::model::enums::SekaiServerRegion,
    engine: &crate::db::engine::DatabaseEngine,
    event_id: i64,
    character_id: i64,
    ranks: Vec<i64>,
    mode: crate::db::query::user::PublicUserIdMode,
) -> Result<BatchAllRankingDataQueryResponseSchema, ApiError> {
    let limiter = state.query_limiter().clone();
    let _permit = limiter.acquire_trace(region).await?;
    let suffix = wb_batch_rank_suffix("trace", character_id, &ranks);
    tracing::debug!(%suffix, "api cache disabled for batch world bloom trace");
    let rankings =
        fetch_all_world_bloom_rankings_by_ranks(engine, event_id, &ranks, character_id, mode)
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
    let items = ranks
        .into_iter()
        .filter_map(|rank| {
            by_rank
                .remove(&rank)
                .map(|rank_data| BatchAllRankingDataItemSchema { rank, rank_data })
        })
        .collect();

    Ok(BatchAllRankingDataQueryResponseSchema { items })
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, rank))]
pub async fn wb_all_by_rank(
    State(state): State<AppState>,
    Path((server, event_id, character_id, rank)): Path<(String, i64, i64, i64)>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
    let limiter = state.query_limiter().clone();
    let fetch = async move {
        let _permit = limiter.acquire_trace(region).await?;
        fetch_wb_rank_trace_response(&engine, event_id, rank, character_id, mode).await
    };
    if let Some(cache) = state.cache() {
        let bytes = cache
            .get_or_fetch_encoded_json(
                &server,
                event_id,
                wb_rank_suffix("trace", character_id, rank),
                cache.ttl(CacheTtl::TraceRank),
                accepts_gzip(&headers),
                fetch,
            )
            .await?;
        Ok(encoded_json(bytes))
    } else {
        raw_json(&fetch.await?).map(|json| EncodedJson::identity(json.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn batch_response_keeps_rank_order_and_skips_missing() {
        let response = batch_response_from_results(vec![
            Ok(Some(BatchAllRankingDataItemSchema {
                rank: 3,
                rank_data: Vec::new(),
            })),
            Ok(None),
            Ok(Some(BatchAllRankingDataItemSchema {
                rank: 1,
                rank_data: Vec::new(),
            })),
        ])
        .unwrap();

        let ranks = response
            .items
            .into_iter()
            .map(|item| item.rank)
            .collect::<Vec<_>>();
        assert_eq!(ranks, vec![3, 1]);
    }

    #[test]
    fn batch_response_returns_not_found_when_all_missing() {
        match batch_response_from_results(vec![Ok(None), Ok(None)]) {
            Err(ApiError::NotFound) => {}
            _ => panic!("expected batch response to return not found"),
        }
    }

    #[test]
    fn accepts_gzip_matches_header_tokens() {
        let mut headers = HeaderMap::new();
        assert!(!accepts_gzip(&headers));

        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("br, gzip;q=1"));
        assert!(accepts_gzip(&headers));

        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("br"));
        assert!(!accepts_gzip(&headers));
    }
}
