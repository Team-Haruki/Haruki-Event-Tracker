//! Rank and player traces: every stored row of one subject, oldest first.
//!
//! A trace only surfaces `(timestamp, public id, score, rank)`, so it reads
//! the ranking table alone — an ordered walk of its `(…, key, time_id)`
//! index (an index-only scan where the covering indexes exist, see
//! `db::schema::create_covering_indexes`) — and looks up the few distinct
//! `user_id_key`s it saw in one small users-table query afterwards. The
//! former joins to the whole time table (for the timestamp) and the whole
//! users table (for the id) hash-joined tens of thousands of rows per trace
//! and sorted the result by the time table's column.
//!
//! The timestamp comes straight from `time_id` once
//! [`time_ids_are_timestamps`] has confirmed that every row of the event's
//! time table has `time_id == timestamp` (what the writer has stored for
//! every sample since `time_id_for_timestamp`, and what `db::repair`
//! restores). A table that passes stays consistent — the writer never
//! stores anything else — so the answer is cached per `(region, event)`
//! for the process's lifetime; a legacy table (sequence ids) keeps the time
//! join and is re-checked now and then, since a repair may have renumbered
//! it. Either way rows are ordered by the ranking table's own `time_id`,
//! which the `time_id` order == `timestamp` order invariant makes the
//! timestamp order.
//!
//! Ids are resolved in Rust with the caller's [`PublicUserIdMode`] — the
//! raw upstream id or the anonymised `unique_id`, never both — so a key
//! whose id column is NULL (or that has no users row, which the old inner
//! join dropped too) yields no row rather than another column's value.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sea_orm::sea_query::{Alias, Expr, Order, Query, SelectStatement, SimpleExpr};
use sea_orm::{DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event_users, time_id};
use crate::db::query::edge::{TimeWindow, and_where_time_id_within};
use crate::db::query::keys::col_in_keys;
use crate::db::query::user::{PublicUserIdMode, user_key_lookup};
use crate::db::query::web::WebTraceFilter;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::{RecordedRankData, RecordedRankingSchema, RecordedWorldBloomRankingSchema};
use crate::model::enums::SekaiServerRegion;

/// How long a legacy verdict stands before the time table is looked at
/// again (a `repair-time-ids` run may have renumbered it).
const LEGACY_RECHECK: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimeIdShape {
    /// `time_id == timestamp` on every row: permanent.
    Timestamps,
    Legacy {
        checked_at: Instant,
    },
}

static TIME_ID_SHAPES: Mutex<Option<HashMap<(SekaiServerRegion, i64), TimeIdShape>>> =
    Mutex::new(None);

fn cached_shape(region: SekaiServerRegion, event_id: i64, now: Instant) -> Option<TimeIdShape> {
    let guard = TIME_ID_SHAPES.lock().unwrap_or_else(|e| e.into_inner());
    let shape = *guard.as_ref()?.get(&(region, event_id))?;
    match shape {
        TimeIdShape::Timestamps => Some(shape),
        TimeIdShape::Legacy { checked_at } => {
            (now.duration_since(checked_at) < LEGACY_RECHECK).then_some(shape)
        }
    }
}

fn remember_shape(region: SekaiServerRegion, event_id: i64, shape: TimeIdShape) {
    let mut guard = TIME_ID_SHAPES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert((region, event_id), shape);
}

/// Drops the cached verdict for an event (tests; a repair could call it).
#[cfg(test)]
pub(crate) fn forget_time_id_shape(region: SekaiServerRegion, event_id: i64) {
    let mut guard = TIME_ID_SHAPES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(&(region, event_id));
    }
}

/// Whether every row of `event_<id>_time_id` has `time_id == timestamp`
/// (see the module doc). A missing table is an error, like any query on
/// it, and is not cached.
pub(crate) async fn time_ids_are_timestamps(
    engine: &DatabaseEngine,
    region: SekaiServerRegion,
    event_id: i64,
) -> Result<bool, DbErr> {
    let now = Instant::now();
    if let Some(shape) = cached_shape(region, event_id, now) {
        return Ok(shape == TimeIdShape::Timestamps);
    }
    let consistent = check_time_ids_are_timestamps(engine, event_id).await?;
    remember_shape(
        region,
        event_id,
        if consistent {
            TimeIdShape::Timestamps
        } else {
            TimeIdShape::Legacy { checked_at: now }
        },
    );
    Ok(consistent)
}

/// The uncached check: `SELECT time_id FROM event_<id>_time_id WHERE
/// time_id <> timestamp LIMIT 1` — the first mismatch stops the scan, so a
/// legacy table answers at once; a consistent one is read fully, once.
pub(crate) async fn check_time_ids_are_timestamps(
    engine: &DatabaseEngine,
    event_id: i64,
) -> Result<bool, DbErr> {
    #[derive(FromQueryResult)]
    struct Mismatch {
        #[allow(dead_code)]
        time_id: i64,
    }
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = Query::select()
        .column((time_tbl.clone(), time_id::Column::TimeId))
        .from(time_tbl.clone())
        .and_where(
            Expr::col((time_tbl.clone(), time_id::Column::TimeId))
                .ne(Expr::col((time_tbl, time_id::Column::Timestamp))),
        )
        .limit(1)
        .to_owned();
    let mismatch = Mismatch::find_by_statement(engine.backend().build(&stmt))
        .one(engine.conn())
        .await?;
    Ok(mismatch.is_none())
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TraceSubject<'a> {
    Rank(i64),
    /// The public id in the query's [`PublicUserIdMode`].
    User(&'a str),
}

#[derive(Debug, FromQueryResult)]
struct TraceRow {
    timestamp: i64,
    user_id_key: i64,
    score: i64,
    rank: i64,
}

#[derive(Debug, FromQueryResult)]
struct WorldBloomTraceRow {
    timestamp: i64,
    user_id_key: i64,
    score: i64,
    rank: i64,
    character_id: i64,
}

/// The trace filters as one inclusive window (the cursor is exclusive).
pub(crate) fn trace_time_window(filter: &WebTraceFilter) -> TimeWindow {
    TimeWindow::new(filter.start_time, filter.end_time)
        .with_start(filter.cursor.and_then(|cursor| cursor.checked_add(1)))
}

pub(crate) fn apply_trace_filters(
    stmt: &mut SelectStatement,
    timestamp_col: SimpleExpr,
    filter: &WebTraceFilter,
) {
    if let Some(start_time) = filter.start_time {
        stmt.and_where(timestamp_col.clone().gte(start_time));
    }
    if let Some(end_time) = filter.end_time {
        stmt.and_where(timestamp_col.clone().lte(end_time));
    }
    if let Some(cursor) = filter.cursor {
        stmt.and_where(timestamp_col.gt(cursor));
    }
}

/// `SELECT <timestamp>, user_id_key, score, rank[, character_id] FROM
/// <ranking table> [JOIN time] WHERE <subject> … ORDER BY time_id`. With
/// `timestamps_direct` the timestamp is the row's `time_id` and the filters
/// apply to it; otherwise the time table supplies it and the filters, with
/// the `time_id` bounds they imply (`and_where_time_id_within`).
fn trace_select(
    event_id: i64,
    character_id: Option<i64>,
    subject: TraceSubject<'_>,
    filter: &WebTraceFilter,
    mode: PublicUserIdMode,
    timestamps_direct: bool,
) -> SelectStatement {
    let tbl = Alias::new(match character_id {
        Some(_) => intern(TableKind::WorldBloom, event_id),
        None => intern(TableKind::Event, event_id),
    });
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let tid_col = Alias::new("time_id");
    let timestamp_col = if timestamps_direct {
        Expr::col((tbl.clone(), tid_col.clone()))
    } else {
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp))
    };

    let mut stmt = Query::select();
    stmt.expr_as(timestamp_col.clone(), Alias::new("timestamp"))
        .expr_as(
            Expr::col((tbl.clone(), Alias::new("user_id_key"))),
            Alias::new("user_id_key"),
        )
        .expr_as(
            Expr::col((tbl.clone(), Alias::new("score"))),
            Alias::new("score"),
        )
        .expr_as(
            Expr::col((tbl.clone(), Alias::new("rank"))),
            Alias::new("rank"),
        );
    if character_id.is_some() {
        stmt.expr_as(
            Expr::col((tbl.clone(), Alias::new("character_id"))),
            Alias::new("character_id"),
        );
    }
    stmt.from(tbl.clone());
    if !timestamps_direct {
        stmt.inner_join(
            time_tbl.clone(),
            Expr::col((tbl.clone(), tid_col.clone()))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        );
    }
    match subject {
        TraceSubject::Rank(rank) => {
            stmt.and_where(Expr::col((tbl.clone(), Alias::new("rank"))).eq(rank));
        }
        TraceSubject::User(user_id) => {
            stmt.and_where(
                Expr::col((tbl.clone(), Alias::new("user_id_key")))
                    .eq(user_key_lookup(event_id, user_id, mode)),
            );
        }
    }
    if let Some(character_id) = character_id {
        stmt.and_where(Expr::col((tbl.clone(), Alias::new("character_id"))).eq(character_id));
    }
    apply_trace_filters(&mut stmt, timestamp_col, filter);
    if !timestamps_direct {
        and_where_time_id_within(
            &mut stmt,
            Expr::col((tbl.clone(), tid_col.clone())),
            intern(TableKind::TimeId, event_id),
            trace_time_window(filter),
        );
    }
    stmt.order_by((tbl, tid_col), Order::Asc);
    if let Some(limit) = filter.limit {
        stmt.limit(limit);
    }
    stmt
}

#[derive(Debug, FromQueryResult)]
struct KeyIdRow {
    user_id_key: i64,
    user_id: Option<String>,
}

/// `user_id_key -> public id` for `keys`, from the users table's column
/// for `mode`. Keys without a row, or whose column is NULL, are absent.
pub(crate) async fn public_ids_by_key(
    engine: &DatabaseEngine,
    event_id: i64,
    keys: &[i64],
    mode: PublicUserIdMode,
) -> Result<HashMap<i64, String>, DbErr> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let stmt = Query::select()
        .column((users_tbl.clone(), event_users::Column::UserIdKey))
        .expr_as(
            Expr::col((users_tbl.clone(), mode.output_column())),
            Alias::new("user_id"),
        )
        .from(users_tbl.clone())
        .and_where(col_in_keys(
            engine.backend(),
            Expr::col((users_tbl, event_users::Column::UserIdKey)),
            keys,
        ))
        .to_owned();
    let rows = KeyIdRow::find_by_statement(engine.backend().build(&stmt))
        .all(engine.conn())
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| Some((row.user_id_key, row.user_id?)))
        .collect())
}

fn distinct_keys(keys: impl Iterator<Item = i64>) -> Vec<i64> {
    let mut keys: Vec<i64> = keys.collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// The subject's rows in `filter`, oldest first, as the API's rank data
/// (World Bloom rows when `character_id` is set).
#[tracing::instrument(skip(engine, filter, subject), fields(region = %region, event_id, character_id))]
pub(crate) async fn fetch_trace(
    engine: &DatabaseEngine,
    region: SekaiServerRegion,
    event_id: i64,
    character_id: Option<i64>,
    subject: TraceSubject<'_>,
    filter: &WebTraceFilter,
    mode: PublicUserIdMode,
) -> Result<Vec<RecordedRankData>, DbErr> {
    let direct = time_ids_are_timestamps(engine, region, event_id).await?;
    let stmt = trace_select(event_id, character_id, subject, filter, mode, direct);
    let built = engine.backend().build(&stmt);
    match character_id {
        None => {
            let rows = TraceRow::find_by_statement(built)
                .all(engine.conn())
                .await?;
            let ids = public_ids_by_key(
                engine,
                event_id,
                &distinct_keys(rows.iter().map(|r| r.user_id_key)),
                mode,
            )
            .await?;
            Ok(rows
                .into_iter()
                .filter_map(|row| {
                    Some(RecordedRankData::Normal(RecordedRankingSchema {
                        timestamp: row.timestamp,
                        user_id: ids.get(&row.user_id_key)?.clone(),
                        score: row.score,
                        rank: row.rank,
                    }))
                })
                .collect())
        }
        Some(_) => {
            let rows = WorldBloomTraceRow::find_by_statement(built)
                .all(engine.conn())
                .await?;
            let ids = public_ids_by_key(
                engine,
                event_id,
                &distinct_keys(rows.iter().map(|r| r.user_id_key)),
                mode,
            )
            .await?;
            Ok(rows
                .into_iter()
                .filter_map(|row| {
                    Some(RecordedRankData::WorldBloom(
                        RecordedWorldBloomRankingSchema {
                            timestamp: row.timestamp,
                            user_id: ids.get(&row.user_id_key)?.clone(),
                            score: row.score,
                            rank: row.rank,
                            character_id: Some(row.character_id),
                        },
                    ))
                })
                .collect())
        }
    }
}

/// The pre-reshape trace (time and users joins, `ORDER BY t.timestamp`),
/// kept as the reference for the equivalence tests.
#[cfg(test)]
pub(crate) mod legacy {
    use sea_orm::sea_query::{Alias, Expr, Order};
    use sea_orm::{DbErr, ExprTrait, FromQueryResult};

    use super::{TraceSubject, apply_trace_filters, trace_time_window};
    use crate::db::engine::DatabaseEngine;
    use crate::db::entity::{event, time_id, world_bloom};
    use crate::db::query::edge::and_where_time_id_within;
    use crate::db::query::user::{PublicUserIdMode, user_key_lookup};
    use crate::db::query::web::WebTraceFilter;
    use crate::db::table_name::{TableKind, intern};
    use crate::model::api::{
        RecordedRankData, RecordedRankingSchema, RecordedWorldBloomRankingSchema,
    };

    pub(crate) async fn fetch_trace(
        engine: &DatabaseEngine,
        event_id: i64,
        character_id: Option<i64>,
        subject: TraceSubject<'_>,
        filter: &WebTraceFilter,
        mode: PublicUserIdMode,
    ) -> Result<Vec<RecordedRankData>, DbErr> {
        let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
        let (mut stmt, tbl) = match character_id {
            Some(_) => (
                crate::db::query::world_bloom::wl_select(event_id, mode),
                Alias::new(intern(TableKind::WorldBloom, event_id)),
            ),
            None => (
                crate::db::query::ranking::ranking_select(event_id, mode),
                Alias::new(intern(TableKind::Event, event_id)),
            ),
        };
        match subject {
            TraceSubject::Rank(rank) => {
                stmt.and_where(Expr::col((tbl.clone(), event::Column::Rank)).eq(rank));
            }
            TraceSubject::User(user_id) => {
                stmt.and_where(
                    Expr::col((tbl.clone(), event::Column::UserIdKey))
                        .eq(user_key_lookup(event_id, user_id, mode)),
                );
            }
        }
        if let Some(character_id) = character_id {
            stmt.and_where(
                Expr::col((tbl.clone(), world_bloom::Column::CharacterId)).eq(character_id),
            );
        }
        apply_trace_filters(
            &mut stmt,
            Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
            filter,
        );
        and_where_time_id_within(
            &mut stmt,
            Expr::col((tbl, event::Column::TimeId)),
            intern(TableKind::TimeId, event_id),
            trace_time_window(filter),
        );
        stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Asc);
        if let Some(limit) = filter.limit {
            stmt.limit(limit);
        }
        let built = engine.backend().build(&stmt);
        Ok(match character_id {
            None => RecordedRankingSchema::find_by_statement(built)
                .all(engine.conn())
                .await?
                .into_iter()
                .map(RecordedRankData::Normal)
                .collect(),
            Some(_) => RecordedWorldBloomRankingSchema::find_by_statement(built)
                .all(engine.conn())
                .await?
                .into_iter()
                .map(RecordedRankData::WorldBloom)
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::*;
    use crate::db::query::web::tests::{
        seed_normal_event_with_history, seed_normal_event_with_inverted_time_ids, sqlite_engine,
    };
    use crate::db::schema::create_event_tables;

    async fn exec(engine: &DatabaseEngine, sql: String) {
        engine
            .conn()
            .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn legacy_verdict_is_rechecked_after_a_repair() {
        let engine = sqlite_engine().await;
        let (region, event_id) = (SekaiServerRegion::Jp, 9601);
        create_event_tables(&engine, region, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_inverted_time_ids(&engine, event_id).await;
        forget_time_id_shape(region, event_id);
        assert!(
            !time_ids_are_timestamps(&engine, region, event_id)
                .await
                .unwrap()
        );
        // Renumber like `db::repair` would; the cached verdict still stands
        // until it ages out, then the table is looked at again.
        let time_tbl = intern(TableKind::TimeId, event_id);
        let event_tbl = intern(TableKind::Event, event_id);
        exec(&engine, format!("DELETE FROM {event_tbl}")).await;
        exec(
            &engine,
            format!("UPDATE {time_tbl} SET time_id = timestamp"),
        )
        .await;
        assert!(
            !time_ids_are_timestamps(&engine, region, event_id)
                .await
                .unwrap()
        );
        remember_shape(
            region,
            event_id,
            TimeIdShape::Legacy {
                checked_at: Instant::now() - LEGACY_RECHECK,
            },
        );
        assert!(
            time_ids_are_timestamps(&engine, region, event_id)
                .await
                .unwrap()
        );
        assert_eq!(
            cached_shape(region, event_id, Instant::now()),
            Some(TimeIdShape::Timestamps)
        );
        forget_time_id_shape(region, event_id);
    }

    #[tokio::test]
    async fn sequence_ids_keep_the_time_join_and_timestamps_skip_it() {
        let engine = sqlite_engine().await;
        let (region, event_id) = (SekaiServerRegion::Jp, 9602);
        create_event_tables(&engine, region, event_id, false)
            .await
            .unwrap();
        // Sequence ids 1, 2 for +0 s and +60 s.
        seed_normal_event_with_history(&engine, event_id).await;
        forget_time_id_shape(region, event_id);
        assert!(
            !time_ids_are_timestamps(&engine, region, event_id)
                .await
                .unwrap()
        );
        assert!(
            !check_time_ids_are_timestamps(&engine, event_id)
                .await
                .unwrap()
        );
        let filter = WebTraceFilter {
            start_time: None,
            end_time: None,
            cursor: None,
            limit: None,
        };
        let trace = fetch_trace(
            &engine,
            region,
            event_id,
            None,
            TraceSubject::Rank(1),
            &filter,
            PublicUserIdMode::Unique,
        )
        .await
        .unwrap();
        let rows: Vec<_> = trace
            .iter()
            .map(|row| match row {
                RecordedRankData::Normal(r) => (r.timestamp, r.user_id.as_str(), r.score),
                RecordedRankData::WorldBloom(_) => panic!("normal rows expected"),
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (1_710_000_000, "u-public-1", 1000),
                (1_710_000_060, "u-public-1", 1300)
            ]
        );
        forget_time_id_shape(region, event_id);

        // A fresh (empty) table is consistent and stays so: the direct form
        // serves it, and rows the writer adds carry their timestamp.
        let event_id = 9603;
        create_event_tables(&engine, region, event_id, false)
            .await
            .unwrap();
        forget_time_id_shape(region, event_id);
        assert!(
            time_ids_are_timestamps(&engine, region, event_id)
                .await
                .unwrap()
        );
        let users_tbl = intern(TableKind::EventUsers, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        let event_tbl = intern(TableKind::Event, event_id);
        exec(
            &engine,
            format!(
                "INSERT INTO {users_tbl} (user_id_key, user_id, unique_id, name) VALUES \
                 (1, '100', 'u-1', 'A'), (2, '200', NULL, 'B')"
            ),
        )
        .await;
        exec(
            &engine,
            format!(
                "INSERT INTO {time_tbl} (time_id, timestamp, status) VALUES \
                 (1710000000, 1710000000, 0), (1710000060, 1710000060, 0)"
            ),
        )
        .await;
        exec(
            &engine,
            format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) VALUES \
                 (1710000000, 1, 10, 1), (1710000060, 2, 20, 1), (1710000120, 3, 30, 1)"
            ),
        )
        .await;
        let trace = |mode| {
            fetch_trace(
                &engine,
                region,
                event_id,
                None,
                TraceSubject::Rank(1),
                &filter,
                mode,
            )
        };
        let ids = |trace: Vec<RecordedRankData>| -> Vec<(i64, String)> {
            trace
                .into_iter()
                .map(|row| match row {
                    RecordedRankData::Normal(r) => (r.timestamp, r.user_id),
                    RecordedRankData::WorldBloom(_) => panic!("normal rows expected"),
                })
                .collect()
        };
        // Raw mode: both players; key 3 has no users row and is dropped.
        assert_eq!(
            ids(trace(PublicUserIdMode::Raw).await.unwrap()),
            vec![(1_710_000_000, "100".into()), (1_710_000_060, "200".into())]
        );
        // Unique mode: player 2 has no unique_id — no row, never the raw id.
        assert_eq!(
            ids(trace(PublicUserIdMode::Unique).await.unwrap()),
            vec![(1_710_000_000, "u-1".into())]
        );
        forget_time_id_shape(region, event_id);
    }
}
