use std::collections::{BTreeMap, BTreeSet};

use crate::api::cache::CacheTtl;
use crate::api::error::ApiError;
use crate::api::extract::{ApiAudience, prepare_audience_user_id_mode, resolve_region_engine};
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::db::query::growth::{
    fetch_ranking_score_growths, fetch_world_bloom_ranking_score_growths,
};
use crate::db::query::user::PublicUserIdMode;
use crate::db::query::web::{
    RankSnapshotCut, is_missing_table_error, latest_rank_cut, rank_snapshot_rows, user_rank_as_of,
    world_bloom_rank_snapshot_rows,
};
use crate::model::api::{
    RankSnapshotSchema, RankSnapshotsResponseSchema, RankingScoreGrowthSchema, WebRankingItemSchema,
};

use super::trace::hashed_subject;
use super::util::{join_ranks, meta, rank_of_item, user_id_of_rank_data};

pub(super) struct SnapshotBuildRequest {
    pub(super) ranks: Vec<i64>,
    pub(super) include_adjacent: bool,
    pub(super) include_metrics: bool,
    pub(super) interval: i64,
    pub(super) at: Option<i64>,
    pub(super) cache_prefix: &'static str,
    pub(super) audience: ApiAudience,
    /// A cut the caller already read other data at (see
    /// [`resolve_user_rank`]); resolved here when `None`.
    pub(super) cut: Option<RankSnapshotCut>,
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
        audience,
        cut,
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
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let cut = match cut {
        Some(cut) => cut,
        None => {
            Box::pin(resolve_rank_cut(
                &state,
                &server,
                &engine,
                event_id,
                character_id,
                at,
            ))
            .await?
        }
    };
    let scope = match character_id {
        Some(character_id) => format!("wb:{character_id}"),
        None => "total".to_owned(),
    };
    let suffix = format!(
        "{cache_prefix}:{scope}:snapshots:ranks={}:adj={include_adjacent}:metrics={include_metrics}:interval={interval}:at={at:?}:cut={:?}:{}",
        join_ranks(&ranks),
        cut.as_of_time_id,
        if include_metrics {
            "lineMetrics=v1"
        } else {
            "lineMetrics=none"
        }
    );
    let cache_server = server.clone();
    let fetch = async {
        let mode =
            prepare_audience_user_id_mode(&state, &engine, region, event_id, audience).await?;
        let current =
            fetch_snapshot_items(&engine, event_id, character_id, &all_ranks, mode, cut).await?;
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
    cut: RankSnapshotCut,
) -> Result<BTreeMap<i64, WebRankingItemSchema>, ApiError> {
    let items: Vec<WebRankingItemSchema> = match character_id {
        Some(character_id) => {
            world_bloom_rank_snapshot_rows(engine, event_id, character_id, ranks, cut, mode)
                .await?
                .into_iter()
                .map(|row| row.into_web_item())
                .collect()
        }
        None => rank_snapshot_rows(engine, event_id, ranks, cut, mode)
            .await?
            .into_iter()
            .map(|row| row.into_web_item())
            .collect(),
    };
    Ok(items
        .into_iter()
        .filter_map(|item| rank_of_item(&item).map(|rank| (rank, item)))
        .collect())
}

/// The as-of cut every "current" rank view of `(server, event, chapter)`
/// should read. An explicit `at` is its own cut (replays are immutable);
/// otherwise it is [`latest_rank_cut`], cached under the event's API-cache
/// epoch so that all requests answered within one epoch — an overview, the
/// rank-N and rank-N±1 lookups a bot issues separately, split endpoints —
/// read the same fully-flushed state even while the replica is already
/// replaying the next flush. Callers put `as_of_time_id` into their own
/// cache keys so a result is never served under another cut.
pub(crate) async fn resolve_rank_cut(
    state: &AppState,
    server: &str,
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    at: Option<i64>,
) -> Result<RankSnapshotCut, ApiError> {
    if at.is_some() {
        return Ok(RankSnapshotCut {
            at,
            as_of_time_id: None,
        });
    }
    // Only a missing table (event not bootstrapped yet) reads as "no cut";
    // any other failure is an error and is never cached as unpinned.
    let fetch = async {
        match latest_rank_cut(engine, event_id, character_id).await {
            Ok(cut) => Ok(cut),
            Err(err) if is_missing_table_error(&err) => Ok(None),
            Err(err) => Err(ApiError::from(err)),
        }
    };
    let as_of_time_id = match state.cache() {
        Some(cache) => {
            let suffix = match character_id {
                Some(character_id) => format!("rankCut:v1:wb:{character_id}"),
                None => "rankCut:v1:total".to_owned(),
            };
            cache
                .get_or_fetch(
                    server,
                    event_id,
                    suffix,
                    cache.ttl(CacheTtl::LatestRank),
                    fetch,
                )
                .await?
        }
        None => fetch.await?,
    };
    Ok(RankSnapshotCut {
        at: None,
        as_of_time_id,
    })
}

/// The rank `user_id` holds at the current cut (or at `at`), together with
/// that cut, so the caller can build the snapshot around it from the same
/// state — a rank looked up from an older state could put another player
/// in `current` and this one beside it.
pub(super) async fn resolve_user_rank(
    state: &AppState,
    server: &str,
    event_id: i64,
    character_id: Option<i64>,
    user_id: &str,
    at: Option<i64>,
    audience: ApiAudience,
) -> Result<(i64, RankSnapshotCut), ApiError> {
    let (region, engine) = resolve_region_engine(state, server)?;
    let cut = Box::pin(resolve_rank_cut(
        state,
        server,
        &engine,
        event_id,
        character_id,
        at,
    ))
    .await?;
    let fetch = async {
        let mode =
            prepare_audience_user_id_mode(state, &engine, region, event_id, audience).await?;
        user_rank_as_of(&engine, event_id, character_id, user_id, cut, mode)
            .await?
            .ok_or(ApiError::NotFound)
    };
    let rank = match state.cache() {
        Some(cache) => {
            // Cloud subjects are raw UIDs: hashed out of the Redis keyspace.
            let subject = match audience {
                ApiAudience::Cloud => hashed_subject(user_id),
                ApiAudience::Web => user_id.to_owned(),
            };
            let scope = match character_id {
                Some(character_id) => format!("wb:{character_id}"),
                None => "total".to_owned(),
            };
            let suffix = format!(
                "userRank:v1:{audience:?}:{scope}:{subject}:at={at:?}:cut={:?}",
                cut.as_of_time_id
            );
            cache
                .get_or_fetch(
                    server,
                    event_id,
                    suffix,
                    cache.ttl(CacheTtl::LatestRank),
                    fetch,
                )
                .await?
        }
        None => fetch.await?,
    };
    Ok((rank, cut))
}

/// A user lookup's snapshot must show that user at the resolved rank. A
/// player who fell out of the tracked ranks keeps their last row, whose rank
/// now belongs to someone else: that is "not currently ranked", not the
/// other player.
pub(super) fn ensure_current_is_user(
    snapshot: &RankSnapshotsResponseSchema,
    rank: i64,
    user_id: &str,
) -> Result<(), ApiError> {
    if snapshot_shows_user(snapshot, rank, user_id) {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

pub(super) fn snapshot_shows_user(
    snapshot: &RankSnapshotsResponseSchema,
    rank: i64,
    user_id: &str,
) -> bool {
    let shown = snapshot
        .items
        .iter()
        .find(|item| item.rank == rank)
        .and_then(|item| item.current.as_ref())
        .and_then(|current| user_id_of_rank_data(&current.rank_data));
    shown.as_deref() == Some(user_id)
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
