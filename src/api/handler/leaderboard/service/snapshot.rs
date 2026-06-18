use std::collections::{BTreeMap, BTreeSet};

use crate::api::cache::CacheTtl;
use crate::api::error::ApiError;
use crate::api::extract::{prepare_user_id_mode, resolve_region_engine};
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::db::query::growth::{
    fetch_ranking_score_growths, fetch_world_bloom_ranking_score_growths,
};
use crate::db::query::user::PublicUserIdMode;
use crate::db::query::web::{
    WebRankingFilter, search_ranking_rows, search_world_bloom_ranking_rows,
};
use crate::model::api::{
    RankSnapshotSchema, RankSnapshotsResponseSchema, RankingScoreGrowthSchema, WebRankingItemSchema,
};

use super::util::{join_ranks, meta, rank_of_item};

pub(super) struct SnapshotBuildRequest {
    pub(super) ranks: Vec<i64>,
    pub(super) include_adjacent: bool,
    pub(super) include_metrics: bool,
    pub(super) interval: i64,
    pub(super) at: Option<i64>,
    pub(super) cache_prefix: &'static str,
}

pub(super) async fn build_rank_snapshots_response(
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
    let end_time = at.unwrap_or_else(|| chrono::Utc::now().timestamp());
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
            "{cache_prefix}:wb:{character_id}:snapshots:ranks={}:adj={include_adjacent}:metrics={include_metrics}:interval={interval}:at={at:?}:{}",
            join_ranks(&ranks),
            if include_metrics {
                "lineMetrics=v1"
            } else {
                "lineMetrics=none"
            }
        ),
        None => format!(
            "{cache_prefix}:total:snapshots:ranks={}:adj={include_adjacent}:metrics={include_metrics}:interval={interval}:at={at:?}:{}",
            join_ranks(&ranks),
            if include_metrics {
                "lineMetrics=v1"
            } else {
                "lineMetrics=none"
            }
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
