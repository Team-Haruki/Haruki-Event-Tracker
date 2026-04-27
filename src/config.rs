use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::model::db_config::DbConfig;
use crate::model::enums::SekaiServerRegion;

pub const DEFAULT_CONFIG_FILE: &str = "haruki-tracker-configs.yaml";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TrackerConfig {
    pub enabled: bool,
    pub use_second_level_cron: bool,
    pub cron: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub password: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SekaiApiConfig {
    pub api_endpoint: String,
    pub api_token: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
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
