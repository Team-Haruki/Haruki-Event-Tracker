//! Per-rank "latest score" lookups for the `/ranking-lines` endpoint
//! (Go: `FetchRankingLines`, `FetchWorldBloomRankingLines`).
//!
//! All ranks are resolved in a single round trip: a `MAX(timestamp) GROUP
//! BY rank` subquery over the ranking ↔ time-id join finds each rank's
//! latest sample, and the outer select joins back for the score. The edge
//! is taken on `timestamp`, never on `time_id`: `time_id` is only an
//! identity, and historical merges have left tables where a later sample
//! carries a smaller id (`timestamp` is unique per event, so the edge
//! resolves to exactly one row). Query errors are swallowed into an empty
//! result — matching the Go reference, which discards goroutine errors and
//! only collects rows that actually came back (and keeping
//! pre-table-bootstrap events a 200).

use std::collections::HashMap;

use sea_orm::sea_query::{Alias, Expr, JoinType, Order, Query, SelectStatement};
use sea_orm::{DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::time_id;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RankingLineScoreSchema;

pub(crate) struct RankEdgeSpec {
    pub tbl: &'static str,
    pub time_tbl: &'static str,
    /// World Bloom chapter filter; `None` on the main event table.
    pub character_id: Option<i64>,
}

pub(crate) enum RankEdge {
    Earliest,
    Latest,
}

/// One row per rank: the earliest/latest `(timestamp, score)` within the
/// optional `[start_time, end_time]` window, resolved via a grouped edge
/// subquery joined back to the ranking and time tables.
pub(crate) fn rank_edge_select(
    spec: &RankEdgeSpec,
    ranks: &[i64],
    edge: RankEdge,
    start_time: Option<i64>,
    end_time: Option<i64>,
) -> SelectStatement {
    let tbl = Alias::new(spec.tbl);
    let time_tbl = Alias::new(spec.time_tbl);
    let edge_tbl = Alias::new("edge");
    let rank_col = Alias::new("rank");
    let score_col = Alias::new("score");
    let tid_col = Alias::new("time_id");
    let edge_ts_col = Alias::new("edge_ts");
    let character_col = Alias::new("character_id");
    let ts_col = || Expr::col((time_tbl.clone(), time_id::Column::Timestamp));
    let time_join = || {
        Expr::col((tbl.clone(), tid_col.clone()))
            .equals((time_tbl.clone(), time_id::Column::TimeId))
    };

    let mut edge_sub = Query::select();
    edge_sub.expr_as(Expr::col((tbl.clone(), rank_col.clone())), rank_col.clone());
    let edge_expr = match edge {
        RankEdge::Earliest => ts_col().min(),
        RankEdge::Latest => ts_col().max(),
    };
    edge_sub
        .expr_as(edge_expr, edge_ts_col.clone())
        .from(tbl.clone())
        .inner_join(time_tbl.clone(), time_join());
    if let Some(start_time) = start_time {
        edge_sub.and_where(ts_col().gte(start_time));
    }
    if let Some(end_time) = end_time {
        edge_sub.and_where(ts_col().lte(end_time));
    }
    edge_sub.and_where(Expr::col((tbl.clone(), rank_col.clone())).is_in(ranks.iter().copied()));
    if let Some(character_id) = spec.character_id {
        edge_sub.and_where(Expr::col((tbl.clone(), character_col.clone())).eq(character_id));
    }
    edge_sub.group_by_col((tbl.clone(), rank_col.clone()));

    let mut stmt = Query::select();
    stmt.expr_as(ts_col(), Alias::new("timestamp"))
        .expr_as(Expr::col((tbl.clone(), score_col)), Alias::new("score"))
        .expr_as(
            Expr::col((tbl.clone(), rank_col.clone())),
            Alias::new("rank"),
        )
        .from(tbl.clone())
        .inner_join(time_tbl.clone(), time_join())
        .join_subquery(
            JoinType::InnerJoin,
            edge_sub.to_owned(),
            edge_tbl.clone(),
            Expr::col((tbl.clone(), rank_col.clone()))
                .equals((edge_tbl.clone(), rank_col.clone()))
                .and(ts_col().equals((edge_tbl, edge_ts_col))),
        );
    if let Some(character_id) = spec.character_id {
        stmt.and_where(Expr::col((tbl.clone(), character_col)).eq(character_id));
    }
    stmt.order_by((tbl, rank_col), Order::Asc).to_owned()
}

/// Runs a rank-edge select and indexes the rows by rank. Errors degrade to
/// an empty map (see the module doc).
pub(crate) async fn fetch_rank_edge_rows(
    engine: &DatabaseEngine,
    stmt: &SelectStatement,
) -> HashMap<i64, RankingLineScoreSchema> {
    let backend = engine.backend();
    match RankingLineScoreSchema::find_by_statement(backend.build(stmt))
        .all(engine.conn())
        .await
    {
        Ok(rows) => rows.into_iter().map(|row| (row.rank, row)).collect(),
        Err(err) => {
            tracing::debug!(%err, "rank edge query failed, returning no rows");
            HashMap::new()
        }
    }
}

async fn fetch_lines(
    engine: &DatabaseEngine,
    spec: RankEdgeSpec,
    ranks: &[i64],
    timestamp: Option<i64>,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    let stmt = rank_edge_select(&spec, ranks, RankEdge::Latest, None, timestamp);
    let mut rows = fetch_rank_edge_rows(engine, &stmt).await;
    Ok(ranks.iter().filter_map(|rank| rows.remove(rank)).collect())
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, ranks_len = ranks.len()))]
pub async fn fetch_ranking_lines(
    engine: &DatabaseEngine,
    event_id: i64,
    ranks: &[i64],
    timestamp: Option<i64>,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    let spec = RankEdgeSpec {
        tbl: intern(TableKind::Event, event_id),
        time_tbl: intern(TableKind::TimeId, event_id),
        character_id: None,
    };
    fetch_lines(engine, spec, ranks, timestamp).await
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, character_id, ranks_len = ranks.len()))]
pub async fn fetch_world_bloom_ranking_lines(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    ranks: &[i64],
    timestamp: Option<i64>,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    let spec = RankEdgeSpec {
        tbl: intern(TableKind::WorldBloom, event_id),
        time_tbl: intern(TableKind::TimeId, event_id),
        character_id: Some(character_id),
    };
    fetch_lines(engine, spec, ranks, timestamp).await
}
