use std::time::Duration;

use sea_orm::{ConnectOptions, Database, DatabaseBackend, DatabaseConnection, DbErr};

use crate::model::db_config::DbConfig;

const DEFAULT_MAX_CONN: u32 = 100;
const DEFAULT_MIN_CONN: u32 = 10;
const DEFAULT_LIFETIME: Duration = Duration::from_secs(3600);

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
}

impl DatabaseEngine {
    /// Connect using a Go-style `DbConfig`. The DSN must be in sqlx URL form
    /// (`postgres://...`, `mysql://...`, `sqlite://...`); the legacy GORM
    /// keyword/`tcp(...)` formats need to be translated by the operator at
    /// cutover time — see `REWRITE_PLAN.md`.
    pub async fn connect(cfg: &DbConfig) -> Result<Self, EngineError> {
        let backend = parse_backend(&cfg.dialect)?;

        let mut opts = ConnectOptions::new(cfg.dsn.clone());
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
        opts.sqlx_logging(false);

        let conn = Database::connect(opts).await.map_err(EngineError::Connect)?;
        Ok(Self { conn, backend })
    }

    pub fn conn(&self) -> &DatabaseConnection {
        &self.conn
    }

    pub fn backend(&self) -> DatabaseBackend {
        self.backend
    }

    pub async fn ping(&self) -> Result<(), DbErr> {
        self.conn.ping().await
    }

    pub async fn close(self) -> Result<(), DbErr> {
        self.conn.close().await
    }
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
    fn parses_durations() {
        assert_eq!(parse_simple_duration("200ms"), Some(Duration::from_millis(200)));
        assert_eq!(parse_simple_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_simple_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_simple_duration("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_simple_duration(""), None);
        assert_eq!(parse_simple_duration("h"), None);
        assert_eq!(parse_simple_duration("1d"), None);
    }

    #[test]
    fn parses_dialects() {
        assert_eq!(parse_backend("MySQL").unwrap(), DatabaseBackend::MySql);
        assert_eq!(parse_backend("postgres").unwrap(), DatabaseBackend::Postgres);
        assert_eq!(parse_backend("postgresql").unwrap(), DatabaseBackend::Postgres);
        assert_eq!(parse_backend("sqlite").unwrap(), DatabaseBackend::Sqlite);
        assert!(matches!(
            parse_backend("nope"),
            Err(EngineError::UnsupportedDialect(_))
        ));
    }
}
