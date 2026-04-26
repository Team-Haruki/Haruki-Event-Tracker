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
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DbConfig {
    pub dialect: String,
    pub dsn: String,
    pub max_open_conns: u32,
    pub max_idle_conns: u32,
    pub conn_max_lifetime: String,
    pub prepare_stmt: bool,
    pub disable_fk_migrate: bool,
    pub logger: DbLoggerConfig,
    pub naming: DbNamingConfig,
}
