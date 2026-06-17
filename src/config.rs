use std::collections::HashMap;
use std::env;
use std::path::Path;

use serde::Deserialize;

use crate::model::db_config::DbConfig;
use crate::model::enums::SekaiServerRegion;
use crate::storage::{StorageError, StorageFile};

pub const DEFAULT_CONFIG_FILE: &str = "haruki-tracker-configs.yaml";
pub const CONFIG_URI_ENV: &str = "HARUKI_CONFIG_URI";
pub const LEGACY_CONFIG_ENV: &str = "HARUKI_CONFIG";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrackerConfig {
    pub enabled: bool,
    pub use_second_level_cron: bool,
    pub cron: String,
    pub post_end_user_refresh_interval_secs: u64,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            use_second_level_cron: false,
            cron: String::new(),
            post_end_user_refresh_interval_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiCacheConfig {
    pub enabled: bool,
    pub redis_url: String,
    pub pool_size: usize,
    pub command_timeout_ms: u64,
    pub local_control_ttl_ms: u64,
    pub local_value_ttl_ms: u64,
    pub local_max_entries: usize,
    pub precompress_gzip_enabled: bool,
    pub precompress_min_bytes: usize,
    pub gzip_level: u32,
    pub default_ttl_secs: u64,
    pub latest_rank_ttl_secs: u64,
    pub trace_rank_ttl_secs: u64,
    pub batch_trace_rank_ttl_secs: u64,
    pub user_data_ttl_secs: u64,
    pub replay_overview_ttl_secs: u64,
    pub negative_ttl_secs: u64,
    pub max_value_bytes: usize,
    pub batch_max_value_bytes: usize,
}

impl Default for ApiCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            redis_url: String::new(),
            pool_size: 2,
            command_timeout_ms: 100,
            local_control_ttl_ms: 100,
            local_value_ttl_ms: 250,
            local_max_entries: 4096,
            precompress_gzip_enabled: true,
            precompress_min_bytes: 4096,
            gzip_level: 1,
            default_ttl_secs: 2,
            latest_rank_ttl_secs: 1,
            trace_rank_ttl_secs: 60,
            batch_trace_rank_ttl_secs: 60,
            user_data_ttl_secs: 30,
            replay_overview_ttl_secs: 3600,
            negative_ttl_secs: 10,
            max_value_bytes: 1024 * 1024,
            batch_max_value_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiQueryConfig {
    pub trace_per_server_max_concurrency: usize,
    pub trace_global_max_concurrency: usize,
    pub acquire_timeout_ms: u64,
    pub batch_trace_fill_concurrency: usize,
}

impl Default for ApiQueryConfig {
    fn default() -> Self {
        Self {
            trace_per_server_max_concurrency: 8,
            trace_global_max_concurrency: 32,
            acquire_timeout_ms: 1500,
            batch_trace_fill_concurrency: 4,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SekaiApiConfig {
    pub api_endpoint: String,
    pub api_token: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UidAnonymizationConfig {
    pub enabled: bool,
    pub salt: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PrivacyConfig {
    pub uid_anonymization: UidAnonymizationConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolboxConfig {
    pub base_url: String,
    pub auth_proxy_secret: String,
    pub authorization: String,
    pub user_agent: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BackendConfig {
    pub host: String,
    pub port: u16,
    pub ssl: bool,
    pub ssl_cert: String,
    pub ssl_key: String,
    pub log_level: String,
    pub main_log_file: String,
    pub access_log: String,
    pub access_log_path: String,
    pub enable_trust_proxy: bool,
    pub trusted_proxies: Vec<String>,
    pub proxy_header: String,
    pub access_log_sample_rate: f64,
    pub access_log_slow_threshold_ms: u64,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 0,
            ssl: false,
            ssl_cert: String::new(),
            ssl_key: String::new(),
            log_level: String::new(),
            main_log_file: String::new(),
            access_log: String::new(),
            access_log_path: String::new(),
            enable_trust_proxy: false,
            trusted_proxies: Vec::new(),
            proxy_header: String::new(),
            access_log_sample_rate: 1.0,
            access_log_slow_threshold_ms: 1000,
        }
    }
}

/// Per-server configuration. The `gorm_config` YAML key is preserved verbatim
/// so old config files keep working through the Go→Rust cutover.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub enabled: bool,
    pub master_data_dir: String,
    pub tracker: TrackerConfig,
    #[serde(rename = "gorm_config")]
    pub db: DbConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub redis: RedisConfig,
    pub api_cache: ApiCacheConfig,
    pub api_query: ApiQueryConfig,
    pub privacy: PrivacyConfig,
    pub toolbox: ToolboxConfig,
    pub backend: BackendConfig,
    pub servers: HashMap<SekaiServerRegion, ServerConfig>,
    #[serde(rename = "sekai_api")]
    pub sekai_api: SekaiApiConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read config file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("read config `{location}`: {source}")]
    ReadStorage {
        location: String,
        #[source]
        source: Box<StorageError>,
    },
    #[error("parse config YAML `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
}

pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let path_ref = path.as_ref();
    let raw = std::fs::read_to_string(path_ref).map_err(|e| ConfigError::Read {
        path: path_ref.display().to_string(),
        source: e,
    })?;
    serde_yaml::from_str(&raw).map_err(|e| ConfigError::Parse {
        path: path_ref.display().to_string(),
        source: e,
    })
}

pub async fn load_from_location(location: &str) -> Result<Config, ConfigError> {
    let raw = StorageFile::from_location(location)
        .map_err(|source| ConfigError::ReadStorage {
            location: location.to_owned(),
            source: Box::new(source),
        })?
        .read_to_string()
        .await
        .map_err(|source| ConfigError::ReadStorage {
            location: location.to_owned(),
            source: Box::new(source),
        })?;
    serde_yaml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: location.to_owned(),
        source,
    })
}

pub fn config_location_from_args_env() -> String {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            if let Some(value) = args.next() {
                return value;
            }
            break;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            return value.to_owned();
        }
        if !arg.starts_with('-') {
            return arg;
        }
    }

    env::var(CONFIG_URI_ENV)
        .or_else(|_| env::var(LEGACY_CONFIG_ENV))
        .unwrap_or_else(|_| DEFAULT_CONFIG_FILE.to_owned())
}
