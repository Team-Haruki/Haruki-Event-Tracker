//! Per-rank "latest score" lookups for the `/ranking-lines` endpoint
//! (Go: `FetchRankingLines`, `FetchWorldBloomRankingLines`).
//!
//! All ranks are resolved in a single round trip: a per-rank `ORDER BY
//! time_id DESC LIMIT 1` probe (`db::query::edge`) finds each rank's latest
//! row on the `(rank, time_id)` index, and the outer select joins back for
//! the score and timestamp.
//! This relies on the invariant that `time_id` order == `timestamp` order
//! (writer: `time_id = timestamp`; legacy rows: `db::repair`).
//! Query errors are swallowed into an empty result — matching the Go
//! reference, which discards goroutine errors and only collects rows that
//! actually came back (and keeping pre-table-bootstrap events a 200).

use std::collections::HashMap;

use sea_orm::sea_query::{Alias, Expr, JoinType, Order, Query, SelectStatement};
use sea_orm::{DatabaseBackend, DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::time_id;
use crate::db::query::edge::{Edge, EdgeSpec, TimeWindow, edge_keys_select};
use crate::db::query::web::RankSnapshotCut;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RankingLineScoreSchema;

pub(crate) struct RankEdgeSpec {
    pub backend: DatabaseBackend,
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
/// optional `[start_time, end_time]` window, resolved via a per-rank edge
/// probe (`db::query::edge`) joined back to the ranking and time tables.
pub(crate) fn rank_edge_select(
    spec: &RankEdgeSpec,
    ranks: &[i64],
    edge: RankEdge,
    start_time: Option<i64>,
    end_time: Option<i64>,
) -> SelectStatement {
    rank_edge_select_until(spec, ranks, edge, start_time, end_time, None)
}

/// [`rank_edge_select`] under a commit cut: rows with `time_id` past
/// `max_time_id` do not exist for the probes (see
/// `db::query::web::latest_rank_cut`).
pub(crate) fn rank_edge_select_until(
    spec: &RankEdgeSpec,
    ranks: &[i64],
    edge: RankEdge,
    start_time: Option<i64>,
    end_time: Option<i64>,
    max_time_id: Option<i64>,
) -> SelectStatement {
    let edge_sub = edge_keys_select(&EdgeSpec {
        backend: spec.backend,
        tbl: spec.tbl,
        time_tbl: spec.time_tbl,
        key_col: "rank",
        keys: ranks,
        character_id: spec.character_id,
        edge: match edge {
            RankEdge::Earliest => Edge::Earliest,
            RankEdge::Latest => Edge::Latest,
        },
        window: TimeWindow::new(start_time, end_time),
        score_min: None,
        score_max: None,
        max_time_id,
    });
    join_rank_edge(spec, edge_sub)
}

fn join_rank_edge(spec: &RankEdgeSpec, edge_sub: SelectStatement) -> SelectStatement {
    let tbl = Alias::new(spec.tbl);
    let time_tbl = Alias::new(spec.time_tbl);
    let edge_tbl = Alias::new("edge");
    let rank_col = Alias::new("rank");
    let score_col = Alias::new("score");
    let tid_col = Alias::new("time_id");
    let character_col = Alias::new("character_id");

    let mut stmt = Query::select();
    stmt.expr_as(
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        Alias::new("timestamp"),
    )
    .expr_as(Expr::col((tbl.clone(), score_col)), Alias::new("score"))
    .expr_as(
        Expr::col((tbl.clone(), rank_col.clone())),
        Alias::new("rank"),
    )
    .from(tbl.clone())
    .join_subquery(
        JoinType::InnerJoin,
        edge_sub,
        edge_tbl.clone(),
        Expr::col((tbl.clone(), rank_col.clone()))
            .equals((edge_tbl.clone(), rank_col.clone()))
            .and(Expr::col((tbl.clone(), tid_col.clone())).equals((edge_tbl, tid_col.clone()))),
    )
    .inner_join(
        time_tbl.clone(),
        Expr::col((tbl.clone(), tid_col)).equals((time_tbl, time_id::Column::TimeId)),
    );
    if let Some(character_id) = spec.character_id {
        stmt.and_where(Expr::col((tbl.clone(), character_col)).eq(character_id));
    }
    stmt.order_by((tbl, rank_col), Order::Asc).to_owned()
}

/// The pre-`edge` grouped form (`MIN/MAX(time_id) GROUP BY rank`), kept as
/// the reference for the equivalence tests and the benchmark.
#[cfg(test)]
pub(crate) fn grouped_rank_edge_select(
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
    let character_col = Alias::new("character_id");

    let mut edge_sub = Query::select();
    edge_sub.expr_as(Expr::col((tbl.clone(), rank_col.clone())), rank_col.clone());
    let edge_expr = match edge {
        RankEdge::Earliest => Expr::col((tbl.clone(), tid_col.clone())).min(),
        RankEdge::Latest => Expr::col((tbl.clone(), tid_col.clone())).max(),
    };
    edge_sub
        .expr_as(edge_expr, tid_col.clone())
        .from(tbl.clone());
    // The time table is only needed to translate time bounds into
    // `time_id`s; without them the grouped edge runs entirely on the
    // `(rank, time_id)` index.
    if start_time.is_some() || end_time.is_some() {
        edge_sub.inner_join(
            time_tbl.clone(),
            Expr::col((tbl.clone(), tid_col.clone()))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        );
        if let Some(start_time) = start_time {
            edge_sub.and_where(
                Expr::col((time_tbl.clone(), time_id::Column::Timestamp)).gte(start_time),
            );
        }
        if let Some(end_time) = end_time {
            edge_sub
                .and_where(Expr::col((time_tbl.clone(), time_id::Column::Timestamp)).lte(end_time));
        }
    }
    edge_sub.and_where(Expr::col((tbl.clone(), rank_col.clone())).is_in(ranks.iter().copied()));
    if let Some(character_id) = spec.character_id {
        edge_sub.and_where(Expr::col((tbl.clone(), character_col.clone())).eq(character_id));
    }
    edge_sub.group_by_col((tbl.clone(), rank_col.clone()));

    let mut stmt = Query::select();
    stmt.expr_as(
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        Alias::new("timestamp"),
    )
    .expr_as(Expr::col((tbl.clone(), score_col)), Alias::new("score"))
    .expr_as(
        Expr::col((tbl.clone(), rank_col.clone())),
        Alias::new("rank"),
    )
    .from(tbl.clone())
    .join_subquery(
        JoinType::InnerJoin,
        edge_sub.to_owned(),
        edge_tbl.clone(),
        Expr::col((tbl.clone(), rank_col.clone()))
            .equals((edge_tbl.clone(), rank_col.clone()))
            .and(Expr::col((tbl.clone(), tid_col.clone())).equals((edge_tbl, tid_col.clone()))),
    )
    .inner_join(
        time_tbl.clone(),
        Expr::col((tbl.clone(), tid_col)).equals((time_tbl, time_id::Column::TimeId)),
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
    cut: RankSnapshotCut,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    let stmt = rank_edge_select_until(
        &spec,
        ranks,
        RankEdge::Latest,
        None,
        cut.at,
        cut.as_of_time_id,
    );
    let mut rows = fetch_rank_edge_rows(engine, &stmt).await;
    Ok(ranks.iter().filter_map(|rank| rows.remove(rank)).collect())
}

/// Each rank's latest `(timestamp, score)` at or before `timestamp`
/// (`None`: its newest row).
#[tracing::instrument(skip(engine, ranks), fields(event_id, ranks_len = ranks.len()))]
pub async fn fetch_ranking_lines(
    engine: &DatabaseEngine,
    event_id: i64,
    ranks: &[i64],
    timestamp: Option<i64>,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    fetch_ranking_lines_at(
        engine,
        event_id,
        ranks,
        RankSnapshotCut {
            at: timestamp,
            as_of_time_id: None,
        },
    )
    .await
}

/// [`fetch_ranking_lines`] reading the state of `cut`, so the lines agree
/// with rank rows read at the same cut (`web::rank_snapshot_rows`) even
/// while the next flush is landing.
#[tracing::instrument(skip(engine, ranks), fields(event_id, ranks_len = ranks.len()))]
pub async fn fetch_ranking_lines_at(
    engine: &DatabaseEngine,
    event_id: i64,
    ranks: &[i64],
    cut: RankSnapshotCut,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    let spec = RankEdgeSpec {
        backend: engine.backend(),
        tbl: intern(TableKind::Event, event_id),
        time_tbl: intern(TableKind::TimeId, event_id),
        character_id: None,
    };
    fetch_lines(engine, spec, ranks, cut).await
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, character_id, ranks_len = ranks.len()))]
pub async fn fetch_world_bloom_ranking_lines(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    ranks: &[i64],
    timestamp: Option<i64>,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    fetch_world_bloom_ranking_lines_at(
        engine,
        event_id,
        character_id,
        ranks,
        RankSnapshotCut {
            at: timestamp,
            as_of_time_id: None,
        },
    )
    .await
}

#[tracing::instrument(skip(engine, ranks), fields(event_id, character_id, ranks_len = ranks.len()))]
pub async fn fetch_world_bloom_ranking_lines_at(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    ranks: &[i64],
    cut: RankSnapshotCut,
) -> Result<Vec<RankingLineScoreSchema>, DbErr> {
    let spec = RankEdgeSpec {
        backend: engine.backend(),
        tbl: intern(TableKind::WorldBloom, event_id),
        time_tbl: intern(TableKind::TimeId, event_id),
        character_id: Some(character_id),
    };
    fetch_lines(engine, spec, ranks, cut).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::query::web::tests::{
        seed_normal_event_with_history, seed_world_bloom_event_with_history, sqlite_engine,
    };
    use crate::db::schema::create_event_tables;
    use crate::model::enums::SekaiServerRegion;

    fn tuples(lines: &[RankingLineScoreSchema]) -> Vec<(i64, i64, i64)> {
        lines
            .iter()
            .map(|line| (line.rank, line.score, line.timestamp))
            .collect()
    }

    #[tokio::test]
    async fn lines_at_a_cut_ignore_rows_past_it() {
        let engine = sqlite_engine().await;
        let (normal, world_bloom) = (571, 572);
        create_event_tables(&engine, SekaiServerRegion::Jp, normal, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, normal).await;
        create_event_tables(&engine, SekaiServerRegion::Jp, world_bloom, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_history(&engine, world_bloom).await;
        // Sequence `time_id`s: the first sample is 1, the second 2.
        let first = RankSnapshotCut {
            at: None,
            as_of_time_id: Some(1),
        };
        let replay = RankSnapshotCut {
            at: Some(1_710_000_030),
            as_of_time_id: None,
        };

        let lines = fetch_ranking_lines_at(&engine, normal, &[1, 3], first)
            .await
            .unwrap();
        assert_eq!(
            tuples(&lines),
            vec![(1, 1000, 1_710_000_000), (3, 800, 1_710_000_000)]
        );
        let lines = fetch_ranking_lines_at(&engine, normal, &[1, 3], replay)
            .await
            .unwrap();
        assert_eq!(
            tuples(&lines),
            vec![(1, 1000, 1_710_000_000), (3, 800, 1_710_000_000)]
        );
        let lines = fetch_ranking_lines_at(&engine, normal, &[1, 3], RankSnapshotCut::default())
            .await
            .unwrap();
        assert_eq!(
            tuples(&lines),
            vec![(1, 1300, 1_710_000_060), (3, 1100, 1_710_000_060)]
        );
        assert_eq!(
            tuples(
                &fetch_ranking_lines(&engine, normal, &[1, 3], None)
                    .await
                    .unwrap()
            ),
            tuples(&lines)
        );

        let lines = fetch_world_bloom_ranking_lines_at(&engine, world_bloom, 17, &[2, 3], first)
            .await
            .unwrap();
        assert_eq!(
            tuples(&lines),
            vec![(2, 1900, 1_710_000_000), (3, 1800, 1_710_000_000)]
        );
        let lines = fetch_world_bloom_ranking_lines_at(
            &engine,
            world_bloom,
            17,
            &[2, 3],
            RankSnapshotCut::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            tuples(&lines),
            vec![(2, 2200, 1_710_000_060), (3, 2100, 1_710_000_060)]
        );
    }
}
