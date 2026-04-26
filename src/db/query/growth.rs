//! Parallel per-rank "score growth over time window" lookups for the
//! `/score-growths` endpoint (Go: `FetchRankingScoreGrowths`,
//! `FetchWorldBloomRankingScoreGrowths`).
//!
//! For each rank we fetch all rows since `start_time` ordered ASC and
//! compute `(latest - earliest)` for both score and timestamp. Ranks with
//! fewer than two rows are skipped. Errors are silently dropped to mirror
//! the Go goroutines.

use futures::future;
use sea_orm::sea_query::{Alias, Expr, Order, Query};
use sea_orm::{DbErr, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, time_id, world_bloom};
use crate::db::table_name::{TableKind, intern};
use crate::model::api::{RankingLineScoreSchema, RankingScoreGrowthSchema};

fn build_growth(rank: i64, rows: Vec<RankingLineScoreSchema>) -> Option<RankingScoreGrowthSchema> {
    if rows.len() < 2 {
        return None;
    }
    let earlier = rows.first()?;
    let latest = rows.last()?;
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

#[tracing::instrument(skip(engine, ranks), fields(event_id, ranks_len = ranks.len(), start_time))]
pub async fn fetch_ranking_score_growths(
    engine: &DatabaseEngine,
    event_id: i64,
    ranks: &[i64],
    start_time: i64,
) -> Result<Vec<RankingScoreGrowthSchema>, DbErr> {
    let backend = engine.backend();
    let event_tbl = intern(TableKind::Event, event_id);
    let time_tbl = intern(TableKind::TimeId, event_id);

    let futs = ranks.iter().copied().map(|rank| {
        let stmt = Query::select()
            .expr_as(
                Expr::col((Alias::new(time_tbl), time_id::Column::Timestamp)),
                Alias::new("timestamp"),
            )
            .expr_as(
                Expr::col((Alias::new(event_tbl), event::Column::Score)),
                Alias::new("score"),
            )
            .expr_as(
                Expr::col((Alias::new(event_tbl), event::Column::Rank)),
                Alias::new("rank"),
            )
            .from(Alias::new(event_tbl))
            .inner_join(
                Alias::new(time_tbl),
                Expr::col((Alias::new(event_tbl), event::Column::TimeId))
                    .equals((Alias::new(time_tbl), time_id::Column::TimeId)),
            )
            .and_where(Expr::col((Alias::new(event_tbl), event::Column::Rank)).eq(rank))
            .and_where(Expr::col((Alias::new(time_tbl), time_id::Column::Timestamp)).gte(start_time))
            .order_by(
                (Alias::new(time_tbl), time_id::Column::Timestamp),
                Order::Asc,
            )
            .to_owned();

        async move {
            let rows = RankingLineScoreSchema::find_by_statement(backend.build(&stmt))
                .all(engine.conn())
                .await;
            (rank, rows)
        }
    });

    let results = future::join_all(futs).await;
    Ok(results
        .into_iter()
        .filter_map(|(rank, res)| res.ok().and_then(|rows| build_growth(rank, rows)))
        .collect())
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, character_id, ranks_len = ranks.len(), start_time))]
pub async fn fetch_world_bloom_ranking_score_growths(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    ranks: &[i64],
    start_time: i64,
) -> Result<Vec<RankingScoreGrowthSchema>, DbErr> {
    let backend = engine.backend();
    let wl_tbl = intern(TableKind::WorldBloom, event_id);
    let time_tbl = intern(TableKind::TimeId, event_id);

    let futs = ranks.iter().copied().map(|rank| {
        let stmt = Query::select()
            .expr_as(
                Expr::col((Alias::new(time_tbl), time_id::Column::Timestamp)),
                Alias::new("timestamp"),
            )
            .expr_as(
                Expr::col((Alias::new(wl_tbl), world_bloom::Column::Score)),
                Alias::new("score"),
            )
            .expr_as(
                Expr::col((Alias::new(wl_tbl), world_bloom::Column::Rank)),
                Alias::new("rank"),
            )
            .from(Alias::new(wl_tbl))
            .inner_join(
                Alias::new(time_tbl),
                Expr::col((Alias::new(wl_tbl), world_bloom::Column::TimeId))
                    .equals((Alias::new(time_tbl), time_id::Column::TimeId)),
            )
            .and_where(Expr::col((Alias::new(wl_tbl), world_bloom::Column::Rank)).eq(rank))
            .and_where(
                Expr::col((Alias::new(wl_tbl), world_bloom::Column::CharacterId)).eq(character_id),
            )
            .and_where(Expr::col((Alias::new(time_tbl), time_id::Column::Timestamp)).gte(start_time))
            .order_by(
                (Alias::new(time_tbl), time_id::Column::Timestamp),
                Order::Asc,
            )
            .to_owned();

        async move {
            let rows = RankingLineScoreSchema::find_by_statement(backend.build(&stmt))
                .all(engine.conn())
                .await;
            (rank, rows)
        }
    });

    let results = future::join_all(futs).await;
    Ok(results
        .into_iter()
        .filter_map(|(rank, res)| res.ok().and_then(|rows| build_growth(rank, rows)))
        .collect())
}
