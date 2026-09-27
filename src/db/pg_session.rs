//! One pooled PostgreSQL connection driven directly, for the writer flush.
//!
//! sea-orm hands out a fresh pooled connection per statement and per
//! transaction, and its transaction handle cannot outlive `COMMIT`, so a
//! flush that wants the users upsert, the ranking transaction and the
//! post-commit `pg_current_wal_lsn()` on *one* connection (one release
//! ping instead of three, one statement cache) has to own the sqlx
//! connection itself. [`PgSession`] wraps it behind sea-orm's
//! `ConnectionTrait` so the statement builders in `db::query::batch` are
//! shared with the SQLite/MySQL path unchanged.
//!
//! [`with_writer_session`] adds the liveness policy: the whole callback
//! runs under the engine's `write_timeout`, and a connection that timed
//! out — or reported itself broken — is detached from the pool and its
//! socket shut down instead of being returned. Returning it would run the
//! pool's release ping on a half-open socket, which hangs without a
//! timeout and pins a pool slot for good.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use futures::lock::Mutex;
use sea_orm::sea_query::Values;
use sea_orm::sqlx::{self, Connection, Executor, PgConnection, Postgres, pool::PoolConnection};
use sea_orm::{
    ConnAcquireErr, ConnectionTrait, DbBackend, DbErr, ExecResult, QueryResult, RuntimeErr,
    Statement,
};
use sea_query_sqlx::SqlxValues;

use crate::db::engine::DatabaseEngine;

fn exec_err(err: sqlx::Error) -> DbErr {
    DbErr::Exec(RuntimeErr::SqlxError(err.into()))
}

fn query_err(err: sqlx::Error) -> DbErr {
    DbErr::Query(RuntimeErr::SqlxError(err.into()))
}

fn acquire_err(err: sqlx::Error) -> DbErr {
    match err {
        sqlx::Error::PoolTimedOut => DbErr::ConnectionAcquire(ConnAcquireErr::Timeout),
        sqlx::Error::PoolClosed => DbErr::ConnectionAcquire(ConnAcquireErr::ConnectionClosed),
        other => DbErr::Conn(RuntimeErr::SqlxError(other.into())),
    }
}

fn bind(stmt: &Statement) -> sqlx::query::Query<'_, Postgres, SqlxValues> {
    let values = stmt.values.clone().unwrap_or_else(|| Values(Vec::new()));
    sqlx::query_with(&stmt.sql, SqlxValues(values))
}

/// One pooled connection, owned for the duration of a flush. Statements
/// run one at a time (the tracker is sequential; the mutex only satisfies
/// `ConnectionTrait`'s `&self` API).
pub struct PgSession {
    conn: Mutex<PoolConnection<Postgres>>,
    broken: AtomicBool,
}

impl PgSession {
    fn new(conn: PoolConnection<Postgres>) -> Self {
        Self {
            conn: Mutex::new(conn),
            broken: AtomicBool::new(false),
        }
    }

    /// Simple-protocol query: several `;`-separated statements in one
    /// round trip, rows of the last one returned. Prepared statements can't
    /// do this, which is what makes `COMMIT; SELECT pg_current_wal_lsn()`
    /// a single round trip. Takes the SQL by value: a borrowed `&str` in a
    /// boxed `Send` future trips rustc's HRTB check on sqlx's `Executor`
    /// (rust-lang/rust #100013).
    pub fn query_simple(&self, sql: String) -> BoxFuture<'_, Result<Vec<QueryResult>, DbErr>> {
        Box::pin(async move {
            let mut guard = self.conn.lock().await;
            let conn: &mut PgConnection = &mut guard;
            conn.fetch_all(sqlx::raw_sql(&sql))
                .await
                .map(|rows| rows.into_iter().map(Into::into).collect())
                .map_err(query_err)
        })
    }

    /// Simple-protocol execute (several `;`-separated statements allowed).
    pub fn execute_simple(&self, sql: String) -> BoxFuture<'_, Result<ExecResult, DbErr>> {
        Box::pin(async move {
            let mut guard = self.conn.lock().await;
            let conn: &mut PgConnection = &mut guard;
            conn.execute(sqlx::raw_sql(&sql))
                .await
                .map(Into::into)
                .map_err(exec_err)
        })
    }

    /// Roll back after a failed statement. If even that fails the
    /// connection is unusable and is marked so [`with_writer_session`]
    /// discards it instead of returning it to the pool.
    pub async fn rollback_or_mark_broken(&self) {
        if let Err(err) = self.execute_simple("ROLLBACK".into()).await {
            tracing::warn!(%err, "rollback failed; discarding the connection");
            self.mark_broken();
        }
    }

    pub fn mark_broken(&self) {
        self.broken.store(true, Ordering::Relaxed);
    }

    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl ConnectionTrait for PgSession {
    fn get_database_backend(&self) -> DbBackend {
        DbBackend::Postgres
    }

    async fn execute_raw(&self, stmt: Statement) -> Result<ExecResult, DbErr> {
        let mut guard = self.conn.lock().await;
        let conn: &mut PgConnection = &mut guard;
        bind(&stmt)
            .execute(conn)
            .await
            .map(Into::into)
            .map_err(exec_err)
    }

    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        self.execute_simple(sql.to_owned()).await
    }

    async fn query_one_raw(&self, stmt: Statement) -> Result<Option<QueryResult>, DbErr> {
        let mut guard = self.conn.lock().await;
        let conn: &mut PgConnection = &mut guard;
        bind(&stmt)
            .fetch_optional(conn)
            .await
            .map(|row| row.map(Into::into))
            .map_err(query_err)
    }

    async fn query_all_raw(&self, stmt: Statement) -> Result<Vec<QueryResult>, DbErr> {
        let mut guard = self.conn.lock().await;
        let conn: &mut PgConnection = &mut guard;
        bind(&stmt)
            .fetch_all(conn)
            .await
            .map(|rows| rows.into_iter().map(Into::into).collect())
            .map_err(query_err)
    }
}

/// Run `f` on one pooled connection of a PostgreSQL engine under the
/// engine's `write_timeout`. The connection goes back to the pool only when
/// `f` finished (Ok or Err) without marking it broken; on timeout or a
/// broken mark it is detached and its socket shut down (no `Terminate`
/// handshake to wait on). `f` receives the session by `Arc` so its future
/// owns no borrow of it — the future is dropped on timeout, which is what
/// lets the connection be reclaimed here.
///
/// Panics if the engine is not PostgreSQL — callers branch on
/// `engine.backend()` first.
pub async fn with_writer_session<T, F, Fut>(engine: &DatabaseEngine, f: F) -> Result<T, DbErr>
where
    F: FnOnce(Arc<PgSession>) -> Fut,
    Fut: Future<Output = Result<T, DbErr>>,
{
    let pool = engine.conn().get_postgres_connection_pool();
    let conn = pool.acquire().await.map_err(acquire_err)?;
    let session = Arc::new(PgSession::new(conn));
    let (result, timed_out) = match engine.write_timeout() {
        Some(limit) => match tokio::time::timeout(limit, f(session.clone())).await {
            Ok(result) => (result, false),
            Err(_) => (
                Err(DbErr::Custom(format!(
                    "database write timed out after {limit:?}"
                ))),
                true,
            ),
        },
        None => (f(session.clone()).await, false),
    };
    let Ok(session) = Arc::try_unwrap(session) else {
        // Unreachable: the callback's future has been consumed or dropped.
        return Err(DbErr::Custom("writer session still in use".into()));
    };
    if timed_out || session.is_broken() {
        tracing::warn!(
            timed_out,
            "discarding writer connection without returning it to the pool"
        );
        let raw = session.conn.into_inner().detach();
        tokio::spawn(async move {
            let _ = raw.close_hard().await;
        });
    }
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, Instant};

    use sea_orm::DatabaseBackend;

    use super::*;
    use crate::db::query::edge::tests::quiet_connect;

    pub(crate) async fn pg_engine(write_timeout: Duration) -> Option<DatabaseEngine> {
        let url = std::env::var("HET_TEST_PG_URL").ok()?;
        Some(
            quiet_connect(&url, DatabaseBackend::Postgres)
                .await
                .with_write_timeout(Some(write_timeout)),
        )
    }

    /// `HET_TEST_PG_URL=postgres://... cargo test --lib -- --ignored
    /// pg_session`.
    #[tokio::test]
    #[ignore = "needs HET_TEST_PG_URL"]
    async fn session_runs_multi_statement_simple_queries_and_returns_to_pool() {
        let Some(engine) = pg_engine(Duration::from_secs(5)).await else {
            return;
        };
        let pool = engine.conn().get_postgres_connection_pool();
        let lsn = with_writer_session(&engine, |s| async move {
            s.execute_unprepared("BEGIN; SET LOCAL statement_timeout = 5000")
                .await?;
            let rows = s
                .query_simple("COMMIT; SELECT pg_current_wal_lsn()::text AS lsn".into())
                .await?;
            rows.last()
                .ok_or_else(|| DbErr::Custom("no lsn row".into()))?
                .try_get::<String>("", "lsn")
        })
        .await
        .unwrap();
        assert!(lsn.contains('/'), "{lsn}");
        // Nothing broke, so the connection went back to the pool.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pool.size(), 1);
        assert_eq!(pool.num_idle(), 1);

        // A statement error rolls back and keeps the connection.
        let err = with_writer_session(&engine, |s| async move {
            s.execute_unprepared("BEGIN").await?;
            let res = s.execute_unprepared("SELECT no_such_column").await;
            if res.is_err() {
                s.rollback_or_mark_broken().await;
            }
            res.map(|_| ())
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no_such_column"), "{err}");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pool.size(), 1);
        assert_eq!(pool.num_idle(), 1);
    }

    #[tokio::test]
    #[ignore = "needs HET_TEST_PG_URL"]
    async fn write_timeout_discards_the_connection_and_frees_the_pool_slot() {
        let Some(engine) = pg_engine(Duration::from_millis(300)).await else {
            return;
        };
        let pool = engine.conn().get_postgres_connection_pool();
        let started = Instant::now();
        let err = with_writer_session(&engine, |s| async move {
            s.execute_unprepared("BEGIN; SELECT pg_sleep(5)")
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        // Detached: the pool no longer counts it, and nothing is waiting
        // on a release ping.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pool.size(), 0);

        // The pool opens a fresh connection and serves immediately.
        let started = Instant::now();
        with_writer_session(&engine, |s| async move {
            s.execute_unprepared("SELECT 1").await.map(|_| ())
        })
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
