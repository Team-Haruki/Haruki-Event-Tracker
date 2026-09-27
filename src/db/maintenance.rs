//! Operator-driven `VACUUM (FREEZE, ANALYZE)` of finished events
//! (`haruki-event-tracker vacuum-finished-events`).
//!
//! Event tables are append-only while the event runs and never written
//! again once it is closed, so autovacuum only ever gets to them through
//! the anti-wraparound path — and tables imported in one batch reach that
//! threshold together, producing one large burst of freeze WAL. Freezing
//! finished events one at a time, off-peak, spreads that cost out and
//! leaves the relations with a fresh `relfrozenxid` so autovacuum has
//! nothing left to do for them.
//!
//! PostgreSQL only; other dialects report "nothing to do". Nothing here is
//! scheduled by the daemon: the subcommand is the only caller.

use std::time::{Duration, Instant};

use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, FromQueryResult, Statement};

use crate::db::engine::DatabaseEngine;
use crate::db::table_name::{TableKind, intern};

/// Master-data `closed_at` is in milliseconds; the time table stores seconds.
const MS_PER_SEC: i64 = 1000;

/// Why an event counts as finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishedReason {
    /// Master data says the event closed at this unix time (seconds).
    ClosedAt(i64),
    /// No master entry was consulted; the tables have simply been idle.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedEvent {
    pub event_id: i64,
    pub reason: FinishedReason,
    /// Newest sample in the time table (unix seconds), if any.
    pub last_sample_at: Option<i64>,
    /// Existing tables of the event, in the order they are vacuumed.
    pub tables: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStats {
    pub table: String,
    pub xid_age: i64,
    pub total_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VacuumedTable {
    pub table: String,
    pub before: TableStats,
    pub after: TableStats,
    pub elapsed: Duration,
}

#[derive(FromQueryResult)]
struct NameRow {
    name: String,
}

#[derive(FromQueryResult)]
struct MaxRow {
    max_ts: Option<i64>,
}

#[derive(FromQueryResult)]
struct StatsRow {
    xid_age: i64,
    total_bytes: i64,
}

#[derive(FromQueryResult)]
struct BoolRow {
    value: bool,
}

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(DatabaseBackend::Postgres, sql)
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Errors early when `engine` cannot be vacuumed: wrong dialect, or a
/// standby (VACUUM must run on the primary; the standby receives it
/// through WAL).
pub async fn check_vacuum_target(engine: &DatabaseEngine) -> Result<(), DbErr> {
    if engine.backend() != DatabaseBackend::Postgres {
        return Err(DbErr::Custom(format!(
            "vacuum-finished-events is PostgreSQL only; this region uses {:?}, nothing to do",
            engine.backend()
        )));
    }
    let row = BoolRow::find_by_statement(pg("SELECT pg_is_in_recovery() AS value"))
        .one(engine.conn())
        .await?;
    if row.is_some_and(|row| row.value) {
        return Err(DbErr::Custom(
            "database is in recovery (a standby); run against the primary DSN".into(),
        ));
    }
    Ok(())
}

/// Event ids that have a time table in the current schema, ascending.
pub async fn list_event_ids(engine: &DatabaseEngine) -> Result<Vec<i64>, DbErr> {
    let rows = NameRow::find_by_statement(pg("SELECT c.relname AS name FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind = 'r' \
         AND c.relname ~ '^event_[0-9]+_time_id$'"))
    .all(engine.conn())
    .await?;
    let mut ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| {
            row.name
                .strip_prefix("event_")?
                .strip_suffix("_time_id")?
                .parse()
                .ok()
        })
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

async fn table_exists<C: ConnectionTrait>(conn: &C, table: &str) -> Result<bool, DbErr> {
    let row = BoolRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind = 'r' AND c.relname = $1) AS value",
        [table.into()],
    ))
    .one(conn)
    .await?;
    Ok(row.is_some_and(|row| row.value))
}

/// The event's existing tables in vacuum order: history first (largest,
/// so a failure surfaces early), then the small ones.
pub async fn event_tables(
    engine: &DatabaseEngine,
    event_id: i64,
) -> Result<Vec<&'static str>, DbErr> {
    let mut tables = Vec::new();
    for kind in [
        TableKind::Event,
        TableKind::WorldBloom,
        TableKind::EventUsers,
        TableKind::TimeId,
    ] {
        let table = intern(kind, event_id);
        if table_exists(engine.conn(), table).await? {
            tables.push(table);
        }
    }
    Ok(tables)
}

/// Newest `timestamp` in the event's time table (unix seconds).
pub async fn last_sample_at(engine: &DatabaseEngine, event_id: i64) -> Result<Option<i64>, DbErr> {
    let table = quote(intern(TableKind::TimeId, event_id));
    let row = MaxRow::find_by_statement(pg(format!(
        "SELECT MAX(\"timestamp\") AS max_ts FROM {table}"
    )))
    .one(engine.conn())
    .await?;
    Ok(row.and_then(|row| row.max_ts))
}

/// Decides whether an event is finished. `closed_at_ms` is the master
/// data entry when one is known; `None` means the caller chose the idle
/// heuristic. Either way the tables must have been idle for `min_idle`,
/// which shields against a stale `events.json` and against events still
/// receiving post-end corrections.
pub fn finished_reason(
    now_secs: i64,
    closed_at_ms: Option<i64>,
    last_sample_at: Option<i64>,
    min_idle: Duration,
) -> Option<FinishedReason> {
    let idle_since = now_secs - min_idle.as_secs() as i64;
    if last_sample_at.is_some_and(|last| last > idle_since) {
        return None;
    }
    match closed_at_ms {
        Some(closed_at_ms) => {
            let closed_at = closed_at_ms / MS_PER_SEC;
            (closed_at <= now_secs && closed_at <= idle_since)
                .then_some(FinishedReason::ClosedAt(closed_at))
        }
        None => Some(FinishedReason::Idle),
    }
}

pub async fn table_stats(engine: &DatabaseEngine, table: &str) -> Result<TableStats, DbErr> {
    let row = StatsRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT age(c.relfrozenxid)::bigint AS xid_age, \
         pg_total_relation_size(c.oid)::bigint AS total_bytes \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind = 'r' AND c.relname = $1",
        [table.into()],
    ))
    .one(engine.conn())
    .await?
    .ok_or_else(|| DbErr::Custom(format!("table {table} does not exist")))?;
    Ok(TableStats {
        table: table.to_owned(),
        xid_age: row.xid_age,
        total_bytes: row.total_bytes,
    })
}

/// `VACUUM (FREEZE, ANALYZE)` one table. Runs outside any transaction
/// (VACUUM refuses to run inside one) on a pooled connection.
pub async fn vacuum_freeze_analyze(
    engine: &DatabaseEngine,
    table: &str,
) -> Result<VacuumedTable, DbErr> {
    let before = table_stats(engine, table).await?;
    let started = Instant::now();
    engine
        .conn()
        .execute_raw(pg(format!("VACUUM (FREEZE, ANALYZE) {}", quote(table))))
        .await?;
    let elapsed = started.elapsed();
    let after = table_stats(engine, table).await?;
    Ok(VacuumedTable {
        table: table.to_owned(),
        before,
        after,
        elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn closed_events_need_master_closed_at_in_the_past_and_an_idle_table() {
        let now = 1_700_000_000;
        // Closed a month ago, last sample two weeks ago.
        assert_eq!(
            finished_reason(
                now,
                Some((now - 30 * 86_400) * 1000),
                Some(now - 14 * 86_400),
                3 * DAY
            ),
            Some(FinishedReason::ClosedAt(now - 30 * 86_400))
        );
        // Closing tomorrow: still live.
        assert_eq!(
            finished_reason(now, Some((now + 86_400) * 1000), Some(now - 10), 3 * DAY),
            None
        );
        // Closed yesterday but the idle guard is three days.
        assert_eq!(
            finished_reason(now, Some((now - 86_400) * 1000), None, 3 * DAY),
            None
        );
        // Master says closed long ago, but something wrote an hour ago.
        assert_eq!(
            finished_reason(
                now,
                Some((now - 30 * 86_400) * 1000),
                Some(now - 3_600),
                3 * DAY
            ),
            None
        );
        // Empty time table with a closed master entry still qualifies.
        assert_eq!(
            finished_reason(now, Some((now - 30 * 86_400) * 1000), None, 3 * DAY),
            Some(FinishedReason::ClosedAt(now - 30 * 86_400))
        );
    }

    #[test]
    fn idle_heuristic_only_looks_at_the_last_sample() {
        let now = 1_700_000_000;
        assert_eq!(
            finished_reason(now, None, Some(now - 8 * 86_400), 7 * DAY),
            Some(FinishedReason::Idle)
        );
        assert_eq!(
            finished_reason(now, None, Some(now - 6 * 86_400), 7 * DAY),
            None
        );
        assert_eq!(
            finished_reason(now, None, None, 7 * DAY),
            Some(FinishedReason::Idle)
        );
    }

    #[tokio::test]
    async fn non_postgres_engines_are_rejected_with_a_clear_message() {
        let conn = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let err = check_vacuum_target(&engine).await.unwrap_err().to_string();
        assert!(err.contains("PostgreSQL only"), "{err}");
        assert!(err.contains("nothing to do"), "{err}");
    }
}
