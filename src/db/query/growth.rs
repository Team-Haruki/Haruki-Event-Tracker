//! Per-rank "score growth over time window" lookups for the
//! `/score-growths` endpoint (Go: `FetchRankingScoreGrowths`,
//! `FetchWorldBloomRankingScoreGrowths`).
//!
//! Each rank only needs the earliest and latest row in the window. Both
//! edges are resolved for all ranks at once — one `MIN(time_id)` and one
//! `MAX(time_id)` grouped subquery joined back to the ranking table — so
//! the whole endpoint costs two round trips instead of two per rank.
//! Ranks with fewer than two rows are skipped. Errors are silently
//! dropped to mirror the Go goroutines.

use sea_orm::DbErr;

use crate::db::engine::DatabaseEngine;
use crate::db::query::lines::{RankEdge, RankEdgeSpec, fetch_rank_edge_rows, rank_edge_select};
use crate::db::table_name::{TableKind, intern};
use crate::model::api::{RankingLineScoreSchema, RankingScoreGrowthSchema};

fn build_growth(
    rank: i64,
    earlier: RankingLineScoreSchema,
    latest: RankingLineScoreSchema,
) -> Option<RankingScoreGrowthSchema> {
    // Same earliest and latest row means the window holds a single sample,
    // which the full-window variant skipped via `rows.len() < 2`.
    if earlier.timestamp == latest.timestamp {
        return None;
    }
    let growth = latest.score - earlier.score;
    let diff = latest.timestamp - earlier.timestamp;
    Some(RankingScoreGrowthSchema {
        rank,
        timestamp_latest: latest.timestamp,
        score_latest: latest.score,
        timestamp_earlier: Some(earlier.timestamp),
        score_earlier: Some(earlier.score),
        time_diff: Some(diff),
        growth: Some(growth),
    })
}

async fn fetch_growths(
    engine: &DatabaseEngine,
    spec: RankEdgeSpec,
    ranks: &[i64],
    start_time: i64,
    end_time: Option<i64>,
) -> Result<Vec<RankingScoreGrowthSchema>, DbErr> {
    let earliest_stmt =
        rank_edge_select(&spec, ranks, RankEdge::Earliest, Some(start_time), end_time);
    let latest_stmt = rank_edge_select(&spec, ranks, RankEdge::Latest, Some(start_time), end_time);
    let (mut earliest, mut latest) = tokio::join!(
        fetch_rank_edge_rows(engine, &earliest_stmt),
        fetch_rank_edge_rows(engine, &latest_stmt),
    );
    Ok(ranks
        .iter()
        .filter_map(|rank| {
            let earlier = earliest.remove(rank)?;
            let latest = latest.remove(rank)?;
            build_growth(*rank, earlier, latest)
        })
        .collect())
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, ranks_len = ranks.len(), start_time))]
pub async fn fetch_ranking_score_growths(
    engine: &DatabaseEngine,
    event_id: i64,
    ranks: &[i64],
    start_time: i64,
    end_time: Option<i64>,
) -> Result<Vec<RankingScoreGrowthSchema>, DbErr> {
    let spec = RankEdgeSpec {
        tbl: intern(TableKind::Event, event_id),
        time_tbl: intern(TableKind::TimeId, event_id),
        character_id: None,
    };
    fetch_growths(engine, spec, ranks, start_time, end_time).await
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, character_id, ranks_len = ranks.len(), start_time))]
pub async fn fetch_world_bloom_ranking_score_growths(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    ranks: &[i64],
    start_time: i64,
    end_time: Option<i64>,
) -> Result<Vec<RankingScoreGrowthSchema>, DbErr> {
    let spec = RankEdgeSpec {
        tbl: intern(TableKind::WorldBloom, event_id),
        time_tbl: intern(TableKind::TimeId, event_id),
        character_id: Some(character_id),
    };
    fetch_growths(engine, spec, ranks, start_time, end_time).await
}
