//! Postgres WAL-position helpers for the cluster deployment. Every function
//! is a no-op (`None` / `true`) on other dialects so the cluster code stays
//! dialect-agnostic.

use std::time::{Duration, Instant};

use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement};

use crate::db::engine::DatabaseEngine;

const REPLAY_POLL: Duration = Duration::from_millis(25);

/// Current flushed WAL position on a primary. Called by the writer right
/// after a commit; with the default `synchronous_commit` the commit record
/// is already flushed, so this LSN covers it.
pub async fn current_wal_lsn(engine: &DatabaseEngine) -> Result<Option<String>, DbErr> {
    if engine.backend() != DatabaseBackend::Postgres {
        return Ok(None);
    }
    let row = engine
        .conn()
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_current_wal_lsn()::text AS lsn",
        ))
        .await?;
    row.map(|row| row.try_get::<String>("", "lsn")).transpose()
}

/// Whether this engine has replayed `lsn`. A primary (not in recovery)
/// trivially has, and so does any non-Postgres engine.
pub async fn replay_reached(engine: &DatabaseEngine, lsn: &str) -> Result<bool, DbErr> {
    if engine.backend() != DatabaseBackend::Postgres {
        return Ok(true);
    }
    let row = engine
        .conn()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT (NOT pg_is_in_recovery()) \
             OR pg_last_wal_replay_lsn() >= $1::pg_lsn AS reached",
            [lsn.into()],
        ))
        .await?;
    Ok(row
        .map(|row| row.try_get::<bool>("", "reached"))
        .transpose()?
        .unwrap_or(true))
}

/// Poll until `lsn` is replayed or `timeout` elapses. Returns whether the
/// position was reached; DB errors count as "not reached" (logged) so a
/// flaky probe can only delay an invalidation, never skip it.
pub async fn wait_for_replay(engine: &DatabaseEngine, lsn: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match replay_reached(engine, lsn).await {
            Ok(true) => return true,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(%err, lsn, "replay probe failed");
                return false;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(REPLAY_POLL).await;
    }
}

/// Seconds since the last replayed transaction on a standby; `None` on a
/// primary or a non-Postgres engine.
pub async fn replication_lag_secs(engine: &DatabaseEngine) -> Result<Option<f64>, DbErr> {
    if engine.backend() != DatabaseBackend::Postgres {
        return Ok(None);
    }
    let row = engine
        .conn()
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT CASE WHEN pg_is_in_recovery() \
             THEN EXTRACT(EPOCH FROM (now() - pg_last_xact_replay_timestamp()))::float8 \
             END AS lag",
        ))
        .await?;
    Ok(row
        .map(|row| row.try_get::<Option<f64>>("", "lag"))
        .transpose()?
        .flatten())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;

    #[tokio::test]
    async fn non_postgres_engines_short_circuit() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        assert_eq!(current_wal_lsn(&engine).await.unwrap(), None);
        assert!(replay_reached(&engine, "0/1").await.unwrap());
        assert!(wait_for_replay(&engine, "0/1", Duration::from_millis(10)).await);
        assert_eq!(replication_lag_secs(&engine).await.unwrap(), None);
    }
}
