use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DbLoggerConfig {
    pub level: String,
    pub slow_threshold: String,
    pub ignore_record_not_found_error: bool,
    pub colorful: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DbNamingConfig {
    pub table_prefix: String,
    pub singular_table: bool,
}

/// Per-server database configuration. The YAML key in `servers.<region>.gorm_config`
/// is preserved verbatim for compatibility with existing config files.
///
/// Durations are Go-style single-unit strings (`1h`, `30m`, `60s`, `200ms`);
/// an empty string means "use the built-in default" (see `db::engine`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DbConfig {
    pub dialect: String,
    pub dsn: String,
    pub max_open_conns: u32,
    pub max_idle_conns: u32,
    pub conn_max_lifetime: String,
    /// How long a pooled connection may sit idle before the pool closes it.
    /// Default 60 s: a burst's extra backends (and their plan caches) are
    /// released soon after the burst instead of lingering for minutes.
    pub idle_timeout: String,
    /// Per-connection prepared-statement cache size (sqlx default is 100).
    /// Each cached statement pins a server-side plan; `None` means the
    /// built-in default of 32, `0` disables caching.
    pub statement_cache_capacity: Option<u32>,
    /// Writer role only: wall-clock cap on one tracker flush (client side)
    /// and, inside the flush transaction, the server-side `statement_timeout`
    /// and `idle_in_transaction_session_timeout`. Default 5 s; `0s` disables.
    pub write_timeout: String,
    /// Accepted for backwards compatibility and ignored: sqlx always uses
    /// prepared statements (see `statement_cache_capacity`).
    pub prepare_stmt: bool,
    pub disable_fk_migrate: bool,
    pub logger: DbLoggerConfig,
    pub naming: DbNamingConfig,
}
