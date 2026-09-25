//! Per-key "first/last row in a time window" lookups whose cost does not
//! grow with the ranking table.
//!
//! The grouped form (`SELECT rank, MAX(time_id) ... GROUP BY rank`) has to
//! visit every history row of every requested key — there is no loose
//! index scan — and with a time filter it also probes the time table for
//! each of those rows. On a per-second tracked top 100 that is the whole
//! table on every request.
//!
//! Here each key is resolved by its own correlated `ORDER BY time_id
//! {ASC|DESC} LIMIT 1` probe on the `(key, time_id)` index, so a lookup
//! costs O(#keys × log n):
//!
//! ```sql
//! SELECT k.rank AS rank,
//!        (SELECT e.time_id FROM event_<id> e
//!           JOIN event_<id>_time_id t ON t.time_id = e.time_id
//!          WHERE e.rank = k.rank
//!            AND e.time_id >= (SELECT time_id FROM event_<id>_time_id
//!                              WHERE timestamp >= $start ORDER BY timestamp LIMIT 1)
//!            AND t.timestamp >= $start
//!          ORDER BY e.time_id DESC LIMIT 1) AS time_id
//!   FROM (SELECT $1 AS rank UNION ALL SELECT $2 ...) k
//! ```
//!
//! The timestamp bounds are translated to `time_id` bounds through the
//! time table's unique `timestamp` index, relying on the invariant that
//! `time_id` order == `timestamp` order (writer: `time_id = timestamp`;
//! legacy rows: `db::repair`). The derived bounds only narrow the index
//! range: the original timestamp predicates are kept, so the result is the
//! grouped form's result whenever the invariant holds, and never contains
//! a row outside the window even if it did not. Legacy events whose ids
//! are sequence numbers (not timestamps) are handled the same way — the
//! bounds are looked up, not computed.
//!
//! The output has the grouped form's shape — one `(key, time_id)` row per
//! key, `time_id` NULL for a key without rows — so callers join it back to
//! the ranking table exactly as before.

use sea_orm::ExprTrait;
use sea_orm::sea_query::{Alias, Expr, Order, Query, SelectStatement, UnionType};

use crate::db::entity::time_id;

/// Aliases private to the correlated edge subquery; callers use their own
/// table names outside it.
const KEYS_ALIAS: &str = "edge_keys";
const INNER_TBL: &str = "edge_e";
const INNER_TIME: &str = "edge_t";
const BOUND_TIME: &str = "edge_b";

/// Inclusive `[start, end]` timestamp window; either side may be open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimeWindow {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl TimeWindow {
    pub fn new(start: Option<i64>, end: Option<i64>) -> Self {
        Self { start, end }
    }

    pub fn is_open(&self) -> bool {
        self.start.is_none() && self.end.is_none()
    }

    /// Tightens the window with another lower bound (`timestamp >= start`).
    pub fn with_start(mut self, start: Option<i64>) -> Self {
        self.start = max_opt(self.start, start);
        self
    }

    /// Tightens the window with another upper bound (`timestamp <= end`).
    pub fn with_end(mut self, end: Option<i64>) -> Self {
        self.end = match (self.end, end) {
            (Some(a), Some(b)) => Some(Ord::min(a, b)),
            (a, b) => a.or(b),
        };
        self
    }
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Ord::max(a, b)),
        (a, b) => a.or(b),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    Earliest,
    Latest,
}

pub(crate) struct EdgeSpec<'a> {
    /// Ranking table (`event_<id>` or `wl_<id>`).
    pub tbl: &'static str,
    pub time_tbl: &'static str,
    /// Key column the probes run on: `rank` or `user_id_key`. The table
    /// must have a `([character_id,] key, time_id)` index.
    pub key_col: &'static str,
    pub keys: &'a [i64],
    /// World Bloom chapter filter; `None` on the main event table.
    pub character_id: Option<i64>,
    pub edge: Edge,
    pub window: TimeWindow,
    pub score_min: Option<i64>,
    pub score_max: Option<i64>,
    /// Commit cut: only rows with `time_id <= max_time_id` exist for this
    /// probe (see `db::query::web::latest_rank_cut`).
    pub max_time_id: Option<i64>,
}

/// `(SELECT time_id FROM <time> WHERE timestamp >= start ORDER BY timestamp
/// LIMIT 1)`: the smallest `time_id` inside a window starting at `start`.
pub(crate) fn time_id_lower_bound(time_tbl: &'static str, start: i64) -> Expr {
    time_id_bound(time_tbl, start, Edge::Earliest)
}

/// The largest `time_id` inside a window ending at `end`.
pub(crate) fn time_id_upper_bound(time_tbl: &'static str, end: i64) -> Expr {
    time_id_bound(time_tbl, end, Edge::Latest)
}

fn time_id_bound(time_tbl: &'static str, ts: i64, edge: Edge) -> Expr {
    let b = Alias::new(BOUND_TIME);
    let ts_col = Expr::col((b.clone(), time_id::Column::Timestamp));
    let mut sub = Query::select();
    sub.column((b.clone(), time_id::Column::TimeId))
        .from_as(Alias::new(time_tbl), b.clone());
    match edge {
        Edge::Earliest => sub
            .and_where(ts_col.gte(ts))
            .order_by((b, time_id::Column::Timestamp), Order::Asc),
        Edge::Latest => sub
            .and_where(ts_col.lte(ts))
            .order_by((b, time_id::Column::Timestamp), Order::Desc),
    };
    sub.limit(1);
    Expr::SubQuery(None, Box::new(sub.to_owned().into()))
}

/// Adds `time_id` range predicates implied by `window` to a query that
/// already filters `time_tbl.timestamp` by the same window, so the ranking
/// table's `(…, time_id)` indexes can range-scan instead of probing the
/// time table for every history row.
pub(crate) fn and_where_time_id_within(
    stmt: &mut SelectStatement,
    time_id_col: Expr,
    time_tbl: &'static str,
    window: TimeWindow,
) {
    if let Some(start) = window.start {
        stmt.and_where(
            time_id_col
                .clone()
                .gte(time_id_lower_bound(time_tbl, start)),
        );
    }
    if let Some(end) = window.end {
        stmt.and_where(time_id_col.lte(time_id_upper_bound(time_tbl, end)));
    }
}

/// `SELECT $1 AS <col> UNION ALL SELECT $2 AS <col> ...`. Portable across
/// PostgreSQL, SQLite and MySQL, unlike `VALUES` lists or `unnest`.
fn key_list(keys: &[i64], col: &str) -> SelectStatement {
    let col = Alias::new(col);
    let mut keys = keys.iter().copied();
    let mut stmt = Query::select();
    let Some(first) = keys.next() else {
        stmt.expr_as(Expr::val(0i64), col)
            .and_where(Expr::val(1i64).eq(0i64));
        return stmt;
    };
    stmt.expr_as(Expr::val(first), col.clone());
    stmt.unions(keys.map(|key| {
        (
            UnionType::All,
            Query::select()
                .expr_as(Expr::val(key), col.clone())
                .to_owned(),
        )
    }));
    stmt
}

/// One `(key, time_id)` row per requested key, `time_id` being the key's
/// first/last row in the window (NULL when it has none).
pub(crate) fn edge_keys_select(spec: &EdgeSpec<'_>) -> SelectStatement {
    let keys_alias = Alias::new(KEYS_ALIAS);
    let e = Alias::new(INNER_TBL);
    let t = Alias::new(INNER_TIME);
    let key_col = Alias::new(spec.key_col);
    let tid_col = Alias::new("time_id");

    let mut probe = Query::select();
    probe
        .column((e.clone(), tid_col.clone()))
        .from_as(Alias::new(spec.tbl), e.clone())
        .and_where(
            Expr::col((e.clone(), key_col.clone())).equals((keys_alias.clone(), key_col.clone())),
        );
    if let Some(character_id) = spec.character_id {
        probe.and_where(Expr::col((e.clone(), Alias::new("character_id"))).eq(character_id));
    }
    // Mirrors the grouped form: the time table is consulted only when there
    // is a window to check, so rows without a time row are treated the same.
    // The timestamp is a scalar per-row lookup rather than a join so the
    // planner cannot turn the probe into a hash join over the whole window:
    // it walks the `(key, time_id)` index inside the derived bounds and stops
    // at the first row whose timestamp passes.
    if !spec.window.is_open() {
        let mut ts_lookup = Query::select();
        ts_lookup
            .column((t.clone(), time_id::Column::Timestamp))
            .from_as(Alias::new(spec.time_tbl), t.clone())
            .and_where(
                Expr::col((t, time_id::Column::TimeId)).equals((e.clone(), tid_col.clone())),
            );
        let ts = Expr::SubQuery(None, Box::new(ts_lookup.into()));
        match (spec.window.start, spec.window.end) {
            (Some(start), Some(end)) => probe.and_where(ts.between(start, end)),
            (Some(start), None) => probe.and_where(ts.gte(start)),
            (None, Some(end)) => probe.and_where(ts.lte(end)),
            (None, None) => unreachable!("window is not open"),
        };
        and_where_time_id_within(
            &mut probe,
            Expr::col((e.clone(), tid_col.clone())),
            spec.time_tbl,
            spec.window,
        );
    }
    if let Some(max_time_id) = spec.max_time_id {
        probe.and_where(Expr::col((e.clone(), tid_col.clone())).lte(max_time_id));
    }
    let score = Expr::col((e.clone(), Alias::new("score")));
    if let Some(score_min) = spec.score_min {
        probe.and_where(score.clone().gte(score_min));
    }
    if let Some(score_max) = spec.score_max {
        probe.and_where(score.lte(score_max));
    }
    probe
        .order_by(
            (e, tid_col.clone()),
            match spec.edge {
                Edge::Earliest => Order::Asc,
                Edge::Latest => Order::Desc,
            },
        )
        .limit(1);

    Query::select()
        .expr_as(Expr::col((keys_alias.clone(), key_col.clone())), key_col)
        .expr_as(Expr::SubQuery(None, Box::new(probe.into())), tid_col)
        .from_subquery(key_list(spec.keys, spec.key_col), keys_alias)
        // Optimisation fence (a no-op limit: one row per key): keeps
        // PostgreSQL from pulling this derived table up into the caller's
        // join, which would evaluate the probe as a join condition — twice
        // per key — instead of once per key. `OFFSET 0` alone is not
        // portable to SQLite.
        .limit(spec.keys.len().max(1) as u64)
        .to_owned()
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod bench;
