use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use sea_orm::{ConnectOptions, Database, DatabaseBackend, DatabaseConnection, DbErr};

use crate::model::db_config::DbConfig;

const DEFAULT_MAX_CONN: u32 = 20;
const DEFAULT_MIN_CONN: u32 = 1;
const DEFAULT_LIFETIME: Duration = Duration::from_secs(3600);
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);
/// 60 s (was 600 s): a reader's burst connections — each a backend holding
/// several MB of cached plans — are reaped a minute after the burst. The
/// writer flushes every few seconds, so its 1–2 connections never idle out.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-connection prepared-statement cache (sqlx default 100). Every cached
/// statement pins a server-side plan; 32 covers the hot statement set.
const DEFAULT_STATEMENT_CACHE_CAPACITY: u32 = 32;
/// Writer flush cap: one hot flush is 6–7 round trips, so 5 s only fires
/// when the link or the primary is actually stuck.
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How the pool is used, which decides the liveness policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineRole {
    /// Standalone and cluster readers: request handlers borrow connections
    /// for one query each, so a stale connection (Postgres restart) must be
    /// caught by the acquire-time ping rather than surface as a 500.
    Serving,
    /// Cluster writer: the tracker is the only user; a failed flush is
    /// retried on the next tick, so the acquire ping is pure latency.
    Writer,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("unsupported database dialect: {0}")]
    UnsupportedDialect(String),
    #[error("database connect: {0}")]
    Connect(#[source] DbErr),
}

pub struct DatabaseEngine {
    conn: DatabaseConnection,
    backend: DatabaseBackend,
    /// Set on cluster readers: the database may be a streaming replica, so
    /// lazy migrations must never issue DDL/DML through this engine.
    read_only: bool,
    /// `None` outside the writer role or when configured to `0s`.
    write_timeout: Option<Duration>,
    /// Per event: whether every `event_<id>_time_id` row has
    /// `time_id == timestamp` (see `time_id_alignment`).
    time_id_alignment: Mutex<HashMap<i64, bool>>,
}

impl DatabaseEngine {
    /// Connect using a Go-style `DbConfig`. The DSN must be in sqlx URL form
    /// (`postgres://...`, `mysql://...`, `sqlite://...`); the legacy GORM
    /// keyword/`tcp(...)` formats need to be translated by the operator at
    /// cutover time — see `REWRITE_PLAN.md`.
    pub async fn connect(cfg: &DbConfig, role: EngineRole) -> Result<Self, EngineError> {
        let backend = parse_backend(&cfg.dialect)?;
        let opts = connect_options(cfg, role);
        let conn = Database::connect(opts)
            .await
            .map_err(EngineError::Connect)?;
        let write_timeout = match role {
            EngineRole::Writer => writer_timeout(&cfg.write_timeout),
            EngineRole::Serving => None,
        };
        Ok(Self {
            conn,
            backend,
            read_only: false,
            write_timeout,
            time_id_alignment: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Wall-clock cap on one tracker flush; `None` disables it (non-writer
    /// roles, or `write_timeout: 0s`).
    pub fn write_timeout(&self) -> Option<Duration> {
        self.write_timeout
    }

    #[cfg(test)]
    pub(crate) fn with_write_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Cached answer to "does every `event_<id>_time_id` row satisfy
    /// `time_id == timestamp`?" — `None` until the writer probes the table.
    /// Only this process writes the table once it is tracking, so the
    /// answer cannot change underneath a running writer (a legacy table
    /// stays legacy until `repair-time-ids` runs, and that is a restart).
    pub fn time_id_alignment(&self, event_id: i64) -> Option<bool> {
        self.time_id_alignment
            .lock()
            .expect("time_id alignment cache poisoned")
            .get(&event_id)
            .copied()
    }

    pub fn set_time_id_alignment(&self, event_id: i64, aligned: bool) {
        self.time_id_alignment
            .lock()
            .expect("time_id alignment cache poisoned")
            .insert(event_id, aligned);
    }

    pub fn conn(&self) -> &DatabaseConnection {
        &self.conn
    }

    pub fn backend(&self) -> DatabaseBackend {
        self.backend
    }

    #[cfg(test)]
    pub(crate) fn from_connection(conn: DatabaseConnection, backend: DatabaseBackend) -> Self {
        Self {
            conn,
            backend,
            read_only: false,
            write_timeout: None,
            time_id_alignment: Mutex::new(HashMap::new()),
        }
    }

    pub async fn ping(&self) -> Result<(), DbErr> {
        self.conn.ping().await
    }

    pub async fn close(self) -> Result<(), DbErr> {
        self.conn.close().await
    }
}

fn connect_options(cfg: &DbConfig, role: EngineRole) -> ConnectOptions {
    let mut opts = ConnectOptions::new(cfg.dsn.clone());
    // Readers keep the acquire-time ping: after a Postgres restart every
    // stale connection would otherwise fail one request each. The writer's
    // flush is retried on the next tick anyway, and the ping is a full
    // round trip on the CN05→CN08 link for every acquire.
    opts.test_before_acquire(role == EngineRole::Serving);
    opts.max_connections(if cfg.max_open_conns > 0 {
        cfg.max_open_conns
    } else {
        DEFAULT_MAX_CONN
    });
    opts.min_connections(if cfg.max_idle_conns > 0 {
        cfg.max_idle_conns
    } else {
        DEFAULT_MIN_CONN
    });
    opts.max_lifetime(parse_simple_duration(&cfg.conn_max_lifetime).unwrap_or(DEFAULT_LIFETIME));
    opts.acquire_timeout(DEFAULT_ACQUIRE_TIMEOUT);
    opts.idle_timeout(parse_simple_duration(&cfg.idle_timeout).unwrap_or(DEFAULT_IDLE_TIMEOUT));
    opts.sqlx_logging(false);

    let cache = cfg
        .statement_cache_capacity
        .unwrap_or(DEFAULT_STATEMENT_CACHE_CAPACITY) as usize;
    opts.map_sqlx_postgres_opts(move |o| o.statement_cache_capacity(cache));
    opts.map_sqlx_mysql_opts(move |o| o.statement_cache_capacity(cache));
    opts.map_sqlx_sqlite_opts(move |o| o.statement_cache_capacity(cache));
    opts
}

fn parse_backend(dialect: &str) -> Result<DatabaseBackend, EngineError> {
    match dialect.trim().to_ascii_lowercase().as_str() {
        "mysql" => Ok(DatabaseBackend::MySql),
        "postgres" | "postgresql" => Ok(DatabaseBackend::Postgres),
        "sqlite" => Ok(DatabaseBackend::Sqlite),
        other => Err(EngineError::UnsupportedDialect(other.to_string())),
    }
}

/// Minimal Go-style duration parser: accepts `<digits><unit>` with units
/// `ns`/`us`/`µs`/`ms`/`s`/`m`/`h`. No composites (`1h30m`) — Go config files
/// in this repo only ever use the single-unit form (`1h`, `200ms`).
/// The writer's flush timeout: `0` (with or without a unit) disables it,
/// an empty or unparseable value keeps the default.
fn writer_timeout(raw: &str) -> Option<Duration> {
    let parsed = if raw.trim() == "0" {
        Some(Duration::ZERO)
    } else {
        parse_simple_duration(raw)
    };
    parsed
        .or(Some(DEFAULT_WRITE_TIMEOUT))
        .filter(|d| !d.is_zero())
}

fn parse_simple_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let (num_str, unit) = s.split_at(split);
    let n: u64 = num_str.parse().ok()?;
    let unit = unit.trim();
    Some(match unit {
        "ns" => Duration::from_nanos(n),
        "us" | "µs" => Duration::from_micros(n),
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.checked_mul(60)?),
        "h" => Duration::from_secs(n.checked_mul(3600)?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_timeout_zero_disables_with_or_without_unit() {
        assert_eq!(writer_timeout("0"), None);
        assert_eq!(writer_timeout(" 0 "), None);
        assert_eq!(writer_timeout("0s"), None);
        assert_eq!(writer_timeout("0ms"), None);
        assert_eq!(writer_timeout(""), Some(DEFAULT_WRITE_TIMEOUT));
        assert_eq!(writer_timeout("junk"), Some(DEFAULT_WRITE_TIMEOUT));
        assert_eq!(writer_timeout("8s"), Some(Duration::from_secs(8)));
    }

    #[test]
    fn parses_durations() {
        assert_eq!(
            parse_simple_duration("200ms"),
            Some(Duration::from_millis(200))
        );
        assert_eq!(parse_simple_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(
            parse_simple_duration("30m"),
            Some(Duration::from_secs(1800))
        );
        assert_eq!(parse_simple_duration("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_simple_duration(""), None);
        assert_eq!(parse_simple_duration("h"), None);
        assert_eq!(parse_simple_duration("1d"), None);
    }

    #[test]
    fn pool_options_follow_config_with_defaults() {
        let opts = connect_options(
            &DbConfig {
                dsn: "postgres://u:p@localhost/db".into(),
                ..DbConfig::default()
            },
            EngineRole::Serving,
        );
        assert_eq!(opts.get_idle_timeout(), Some(Some(DEFAULT_IDLE_TIMEOUT)));
        assert_eq!(opts.get_max_lifetime(), Some(Some(DEFAULT_LIFETIME)));
        assert_eq!(opts.get_max_connections(), Some(DEFAULT_MAX_CONN));
        assert!(
            opts.sqlx_pool_options::<sea_orm::sqlx::Postgres>()
                .get_test_before_acquire()
        );

        let opts = connect_options(
            &DbConfig {
                dsn: "postgres://u:p@localhost/db".into(),
                idle_timeout: "2m".into(),
                conn_max_lifetime: "30m".into(),
                max_open_conns: 6,
                ..DbConfig::default()
            },
            EngineRole::Writer,
        );
        assert!(
            !opts
                .clone()
                .sqlx_pool_options::<sea_orm::sqlx::Postgres>()
                .get_test_before_acquire()
        );
        assert_eq!(
            opts.get_idle_timeout(),
            Some(Some(Duration::from_secs(120)))
        );
        assert_eq!(
            opts.get_max_lifetime(),
            Some(Some(Duration::from_secs(1800)))
        );
        assert_eq!(opts.get_max_connections(), Some(6));
    }

    #[tokio::test]
    async fn write_timeout_applies_to_the_writer_role_only() {
        let cfg = DbConfig {
            dialect: "sqlite".into(),
            dsn: "sqlite::memory:".into(),
            ..DbConfig::default()
        };
        let engine = DatabaseEngine::connect(&cfg, EngineRole::Writer)
            .await
            .unwrap();
        assert_eq!(engine.write_timeout(), Some(DEFAULT_WRITE_TIMEOUT));
        let engine = DatabaseEngine::connect(&cfg, EngineRole::Serving)
            .await
            .unwrap();
        assert_eq!(engine.write_timeout(), None);
        let engine = DatabaseEngine::connect(
            &DbConfig {
                write_timeout: "0s".into(),
                ..cfg.clone()
            },
            EngineRole::Writer,
        )
        .await
        .unwrap();
        assert_eq!(engine.write_timeout(), None);
        let engine = DatabaseEngine::connect(
            &DbConfig {
                write_timeout: "750ms".into(),
                ..cfg
            },
            EngineRole::Writer,
        )
        .await
        .unwrap();
        assert_eq!(engine.write_timeout(), Some(Duration::from_millis(750)));

        assert_eq!(engine.time_id_alignment(1), None);
        engine.set_time_id_alignment(1, true);
        assert_eq!(engine.time_id_alignment(1), Some(true));
    }

    #[test]
    fn parses_dialects() {
        assert_eq!(parse_backend("MySQL").unwrap(), DatabaseBackend::MySql);
        assert_eq!(
            parse_backend("postgres").unwrap(),
            DatabaseBackend::Postgres
        );
        assert_eq!(
            parse_backend("postgresql").unwrap(),
            DatabaseBackend::Postgres
        );
        assert_eq!(parse_backend("sqlite").unwrap(), DatabaseBackend::Sqlite);
        assert!(matches!(
            parse_backend("nope"),
            Err(EngineError::UnsupportedDialect(_))
        ));
    }
}
