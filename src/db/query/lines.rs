//! Parallel per-rank "latest score" lookups for the `/ranking-lines`
//! endpoint (Go: `FetchRankingLines`, `FetchWorldBloomRankingLines`).
//!
//! Per-rank query errors are silently dropped — matching the Go reference,
//! which discards goroutine errors and only collects rows that actually
//! came back.

use futures::future;
use sea_orm::sea_query::{Alias, Expr, Order, Query};
use sea_orm::{DbErr, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, time_id, world_bloom};
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RankingLineScoreSchema;

#[tracing::instrument(skip(engine, ranks), fields(event_id, ranks_len = ranks.len()))]
pub async fn fetch_ranking_lines(
    engine: &DatabaseEngine,
    event_id: i64,
    ranks: &[i64],
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
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
            .order_by(
                (Alias::new(time_tbl), time_id::Column::Timestamp),
                Order::Desc,
            )
            .limit(1)
            .to_owned();

        async move {
            RankingLineScoreSchema::find_by_statement(backend.build(&stmt))
                .one(engine.conn())
                .await
        }
    });

    let results = future::join_all(futs).await;
    Ok(results.into_iter().filter_map(|r| r.ok().flatten()).collect())
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, character_id, ranks_len = ranks.len()))]
pub async fn fetch_world_bloom_ranking_lines(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    ranks: &[i64],
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
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
            .order_by(
                (Alias::new(time_tbl), time_id::Column::Timestamp),
                Order::Desc,
            )
            .limit(1)
            .to_owned();

        async move {
            RankingLineScoreSchema::find_by_statement(backend.build(&stmt))
                .one(engine.conn())
                .await
        }
    });

    let results = future::join_all(futs).await;
    Ok(results.into_iter().filter_map(|r| r.ok().flatten()).collect())
}
