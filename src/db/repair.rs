//! One-shot repair of the `time_id` / `timestamp` order invariant for one
//! event (`haruki-event-tracker repair-time-ids`).
//!
//! Readers take "latest per rank" as `MAX(time_id)` on the ranking-table
//! indexes, so every `event_<id>_time_id` row must be ordered by id exactly
//! as by timestamp. The writer guarantees that for its own rows
//! (`time_id = timestamp`); rows that predate that rule carry sequence ids,
//! and historical merges have inserted them out of timestamp order. This
//! module detects such inversions and, when there are any, renumbers the
//! whole event to `time_id = timestamp` — the time table plus every table
//! that references it (`event_<id>`, `wl_<id>`) — in one transaction.
//!
//! The renumber is two-phase through negative ids so no intermediate state
//! can collide with an existing primary key, whatever the id/timestamp
//! ranges are. Ranking rows whose `time_id` has no time row (there is no
//! foreign key) are left untouched and reported. A second run finds no
//! inversions and changes nothing. Identity sequences are not touched.
//!
//! Supported on PostgreSQL (production) and SQLite (tests); MySQL is
//! untested.

use sea_orm::{
    ConnectionTrait, DatabaseBackend, DbErr, FromQueryResult, Statement, TransactionTrait,
};

use crate::db::engine::DatabaseEngine;
use crate::db::table_name::{TableKind, intern};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// Time rows whose timestamp is older than that of some smaller id.
    pub inversions: u64,
    /// Time rows with `time_id != timestamp` before the run.
    pub drifted_time_rows: u64,
    pub renumbered_time_rows: u64,
    pub renumbered_ranking_rows: u64,
    pub renumbered_world_bloom_rows: u64,
    /// Ranking rows referencing a `time_id` that has no time row; skipped.
    pub orphan_ranking_rows: u64,
    pub orphan_world_bloom_rows: u64,
    pub applied: bool,
}

#[derive(FromQueryResult)]
struct CountRow {
    n: i64,
}

fn quote(backend: DatabaseBackend, ident: &str) -> String {
    match backend {
        DatabaseBackend::MySql => format!("`{ident}`"),
        _ => format!("\"{ident}\""),
    }
}

async fn count<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    sql: String,
) -> Result<u64, DbErr> {
    let row = CountRow::find_by_statement(Statement::from_string(backend, sql))
        .one(conn)
        .await?
        .ok_or_else(|| DbErr::Custom("count query returned no row".into()))?;
    Ok(u64::try_from(row.n).unwrap_or(0))
}

async fn table_exists<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    table: &str,
) -> Result<bool, DbErr> {
    let sql = match backend {
        DatabaseBackend::Sqlite => format!(
            "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
        ),
        DatabaseBackend::Postgres => format!(
            "SELECT COUNT(*) AS n FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_name = '{table}'"
        ),
        DatabaseBackend::MySql => format!(
            "SELECT COUNT(*) AS n FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = '{table}'"
        ),
        other => {
            return Err(DbErr::Custom(format!(
                "repair-time-ids does not support backend {other:?}"
            )));
        }
    };
    Ok(count(conn, backend, sql).await? > 0)
}

async fn count_inversions<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    time_tbl: &str,
) -> Result<u64, DbErr> {
    let t = quote(backend, time_tbl);
    let id = quote(backend, "time_id");
    let ts = quote(backend, "timestamp");
    count(
        conn,
        backend,
        format!(
            "SELECT COUNT(*) AS n FROM (SELECT {ts} AS ts, MAX({ts}) OVER (ORDER BY {id} \
             ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS prev_max FROM {t}) x \
             WHERE x.prev_max > x.ts"
        ),
    )
    .await
}

async fn count_drift<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    time_tbl: &str,
) -> Result<u64, DbErr> {
    let t = quote(backend, time_tbl);
    let id = quote(backend, "time_id");
    let ts = quote(backend, "timestamp");
    count(
        conn,
        backend,
        format!("SELECT COUNT(*) AS n FROM {t} WHERE {id} <> {ts}"),
    )
    .await
}

async fn count_orphans<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    ref_tbl: &str,
    time_tbl: &str,
) -> Result<u64, DbErr> {
    let r = quote(backend, ref_tbl);
    let t = quote(backend, time_tbl);
    let id = quote(backend, "time_id");
    count(
        conn,
        backend,
        format!(
            "SELECT COUNT(*) AS n FROM {r} WHERE NOT EXISTS \
             (SELECT 1 FROM {t} WHERE {t}.{id} = {r}.{id})"
        ),
    )
    .await
}

async fn exec<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    sql: String,
) -> Result<u64, DbErr> {
    Ok(conn
        .execute_raw(Statement::from_string(backend, sql))
        .await?
        .rows_affected())
}

/// Phase 1 for a referencing table: rows pointing at a time row that will
/// move get the *negative* target id. Rows already at `time_id = timestamp`
/// and orphans are untouched.
async fn stage_referencing_rows<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    ref_tbl: &str,
    time_tbl: &str,
) -> Result<u64, DbErr> {
    let r = quote(backend, ref_tbl);
    let t = quote(backend, time_tbl);
    let id = quote(backend, "time_id");
    let ts = quote(backend, "timestamp");
    exec(
        conn,
        backend,
        format!(
            "UPDATE {r} SET {id} = -(SELECT {t}.{ts} FROM {t} WHERE {t}.{id} = {r}.{id}) \
             WHERE EXISTS (SELECT 1 FROM {t} WHERE {t}.{id} = {r}.{id} AND {t}.{id} <> {t}.{ts})"
        ),
    )
    .await
}

async fn stage_time_rows<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    time_tbl: &str,
) -> Result<u64, DbErr> {
    let t = quote(backend, time_tbl);
    let id = quote(backend, "time_id");
    let ts = quote(backend, "timestamp");
    exec(
        conn,
        backend,
        format!("UPDATE {t} SET {id} = -{ts} WHERE {id} <> {ts}"),
    )
    .await
}

/// Phase 2: flip every staged (negative) id to its final value.
async fn commit_staged_rows<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    tbl: &str,
) -> Result<u64, DbErr> {
    let t = quote(backend, tbl);
    let id = quote(backend, "time_id");
    exec(
        conn,
        backend,
        format!("UPDATE {t} SET {id} = -{id} WHERE {id} < 0"),
    )
    .await
}

/// Detect inversions for `event_id` and, unless `dry_run` or there are
/// none, renumber the event's time ids to `time_id = timestamp` in one
/// transaction. Idempotent.
#[tracing::instrument(skip(engine), fields(event_id, dry_run))]
pub async fn repair_time_ids(
    engine: &DatabaseEngine,
    event_id: i64,
    dry_run: bool,
) -> Result<RepairReport, DbErr> {
    let backend = engine.backend();
    let time_tbl = intern(TableKind::TimeId, event_id);
    let event_tbl = intern(TableKind::Event, event_id);
    let wl_tbl = intern(TableKind::WorldBloom, event_id);

    let tx = engine.conn().begin().await?;
    if !table_exists(&tx, backend, time_tbl).await? {
        return Err(DbErr::Custom(format!("table {time_tbl} does not exist")));
    }
    let has_event = table_exists(&tx, backend, event_tbl).await?;
    let has_wl = table_exists(&tx, backend, wl_tbl).await?;

    let mut report = RepairReport {
        inversions: count_inversions(&tx, backend, time_tbl).await?,
        drifted_time_rows: count_drift(&tx, backend, time_tbl).await?,
        ..RepairReport::default()
    };
    if has_event {
        report.orphan_ranking_rows = count_orphans(&tx, backend, event_tbl, time_tbl).await?;
    }
    if has_wl {
        report.orphan_world_bloom_rows = count_orphans(&tx, backend, wl_tbl, time_tbl).await?;
    }
    if dry_run || report.inversions == 0 {
        tx.rollback().await?;
        return Ok(report);
    }

    if has_event {
        report.renumbered_ranking_rows =
            stage_referencing_rows(&tx, backend, event_tbl, time_tbl).await?;
    }
    if has_wl {
        report.renumbered_world_bloom_rows =
            stage_referencing_rows(&tx, backend, wl_tbl, time_tbl).await?;
    }
    report.renumbered_time_rows = stage_time_rows(&tx, backend, time_tbl).await?;
    for tbl in [
        Some(time_tbl),
        has_event.then_some(event_tbl),
        has_wl.then_some(wl_tbl),
    ]
    .into_iter()
    .flatten()
    {
        commit_staged_rows(&tx, backend, tbl).await?;
    }
    let remaining = count_inversions(&tx, backend, time_tbl).await?;
    if remaining != 0 {
        tx.rollback().await?;
        return Err(DbErr::Custom(format!(
            "{remaining} inversions remain after renumbering; rolled back"
        )));
    }
    tx.commit().await?;
    report.applied = true;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use sea_orm::{Database, DatabaseBackend};

    use super::*;
    use crate::db::query::lines::{fetch_ranking_lines, fetch_world_bloom_ranking_lines};
    use crate::db::query::user::PublicUserIdMode;
    use crate::db::query::web::tests::{
        rank_window, seed_normal_event_with_inverted_time_ids,
        seed_world_bloom_event_with_inverted_time_ids,
    };
    use crate::db::query::web::{search_rankings, search_world_bloom_rankings};
    use crate::db::schema::create_event_tables;
    use crate::model::api::RecordedRankData;
    use crate::model::enums::SekaiServerRegion;

    #[derive(FromQueryResult)]
    struct IdRow {
        time_id: i64,
        timestamp: i64,
    }

    async fn engine() -> DatabaseEngine {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite)
    }

    async fn time_rows(engine: &DatabaseEngine, event_id: i64) -> Vec<(i64, i64)> {
        let sql = format!(
            "SELECT time_id, timestamp FROM {} ORDER BY time_id",
            intern(TableKind::TimeId, event_id)
        );
        IdRow::find_by_statement(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .all(engine.conn())
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.time_id, r.timestamp))
            .collect()
    }

    async fn ref_time_ids(engine: &DatabaseEngine, table: &str) -> Vec<i64> {
        let sql = format!("SELECT time_id, 0 AS timestamp FROM {table} ORDER BY time_id");
        IdRow::find_by_statement(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .all(engine.conn())
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.time_id)
            .collect()
    }

    #[tokio::test]
    async fn dry_run_reports_inversions_without_changing_anything() {
        let engine = engine().await;
        let event_id = 8101;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_inverted_time_ids(&engine, event_id).await;
        let before = time_rows(&engine, event_id).await;

        let report = repair_time_ids(&engine, event_id, true).await.unwrap();
        assert_eq!(
            report,
            RepairReport {
                inversions: 2,
                drifted_time_rows: 3,
                ..RepairReport::default()
            }
        );
        assert_eq!(time_rows(&engine, event_id).await, before);
    }

    #[tokio::test]
    async fn renumbers_time_and_ranking_rows_so_max_time_id_is_latest() {
        let engine = engine().await;
        let event_id = 8102;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_inverted_time_ids(&engine, event_id).await;
        // An orphan row (no time row) and a row already at id == timestamp.
        let event_tbl = intern(TableKind::Event, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        for sql in [
            format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) VALUES (99, 1, 1, 50)"
            ),
            format!(
                "INSERT INTO {time_tbl} (time_id, timestamp, status) VALUES (1710000090, 1710000090, 0)"
            ),
            format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) VALUES (1710000090, 2, 1400, 1)"
            ),
        ] {
            engine
                .conn()
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await
                .unwrap();
        }
        // Before the repair, id order lies: rank 1's newest id is the +90s
        // row, but among the legacy rows the +30s sample outranks +60s.
        let lines = fetch_ranking_lines(&engine, event_id, &[2], None)
            .await
            .unwrap();
        assert_eq!((lines[0].timestamp, lines[0].score), (1_710_000_030, 1050));

        let report = repair_time_ids(&engine, event_id, false).await.unwrap();
        assert_eq!(
            report,
            RepairReport {
                inversions: 2,
                drifted_time_rows: 3,
                renumbered_time_rows: 3,
                renumbered_ranking_rows: 6,
                orphan_ranking_rows: 1,
                applied: true,
                ..RepairReport::default()
            }
        );
        assert_eq!(
            time_rows(&engine, event_id).await,
            vec![
                (1_710_000_000, 1_710_000_000),
                (1_710_000_030, 1_710_000_030),
                (1_710_000_060, 1_710_000_060),
                (1_710_000_090, 1_710_000_090),
            ]
        );
        assert_eq!(
            ref_time_ids(&engine, event_tbl).await,
            vec![
                99,
                1_710_000_000,
                1_710_000_000,
                1_710_000_030,
                1_710_000_030,
                1_710_000_060,
                1_710_000_060,
                1_710_000_090,
            ]
        );

        let lines = fetch_ranking_lines(&engine, event_id, &[1, 2], None)
            .await
            .unwrap();
        let rows: Vec<_> = lines
            .iter()
            .map(|l| (l.rank, l.timestamp, l.score))
            .collect();
        assert_eq!(
            rows,
            vec![(1, 1_710_000_090, 1400), (2, 1_710_000_060, 1200)]
        );
        let (items, _) = search_rankings(
            &engine,
            event_id,
            &rank_window(1, 2),
            PublicUserIdMode::Unique,
        )
        .await
        .unwrap();
        let rows: Vec<_> = items
            .into_iter()
            .map(|item| match item.rank_data {
                RecordedRankData::Normal(row) => (row.rank, row.timestamp, row.score),
                RecordedRankData::WorldBloom(_) => panic!("expected normal ranking"),
            })
            .collect();
        assert_eq!(
            rows,
            vec![(1, 1_710_000_090, 1400), (2, 1_710_000_060, 1200)]
        );

        // Idempotent: nothing left to do.
        let again = repair_time_ids(&engine, event_id, false).await.unwrap();
        assert_eq!(
            again,
            RepairReport {
                orphan_ranking_rows: 1,
                ..RepairReport::default()
            }
        );
    }

    #[tokio::test]
    async fn renumbers_world_bloom_rows_too() {
        let engine = engine().await;
        let event_id = 8103;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_inverted_time_ids(&engine, event_id).await;

        let report = repair_time_ids(&engine, event_id, false).await.unwrap();
        assert_eq!(report.inversions, 2);
        assert_eq!(report.renumbered_time_rows, 3);
        assert_eq!(report.renumbered_world_bloom_rows, 7);
        assert!(report.applied);

        let lines = fetch_world_bloom_ranking_lines(&engine, event_id, 17, &[1, 2], None)
            .await
            .unwrap();
        let rows: Vec<_> = lines
            .iter()
            .map(|l| (l.rank, l.timestamp, l.score))
            .collect();
        assert_eq!(
            rows,
            vec![(1, 1_710_000_060, 2300), (2, 1_710_000_060, 2200)]
        );
        let (items, _) = search_world_bloom_rankings(
            &engine,
            event_id,
            17,
            &rank_window(1, 2),
            PublicUserIdMode::Unique,
        )
        .await
        .unwrap();
        let rows: Vec<_> = items
            .into_iter()
            .map(|item| match item.rank_data {
                RecordedRankData::WorldBloom(row) => (row.rank, row.timestamp, row.score),
                RecordedRankData::Normal(_) => panic!("expected world bloom ranking"),
            })
            .collect();
        assert_eq!(
            rows,
            vec![(1, 1_710_000_060, 2300), (2, 1_710_000_060, 2200)]
        );
    }

    #[tokio::test]
    async fn legacy_ids_in_order_are_left_alone() {
        let engine = engine().await;
        let event_id = 8104;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        crate::db::query::web::tests::seed_normal_event_with_history(&engine, event_id).await;
        let before = time_rows(&engine, event_id).await;
        let report = repair_time_ids(&engine, event_id, false).await.unwrap();
        assert_eq!(
            report,
            RepairReport {
                drifted_time_rows: 2,
                ..RepairReport::default()
            }
        );
        assert_eq!(time_rows(&engine, event_id).await, before);
    }

    #[tokio::test]
    async fn missing_event_is_an_error() {
        let engine = engine().await;
        assert!(repair_time_ids(&engine, 8105, true).await.is_err());
    }
}
