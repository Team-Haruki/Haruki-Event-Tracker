//! Bootstrap: Redis → Sekai API client → per-server DB engines →
//! tracker daemons → cron scheduler. Mirrors `api.InitAPIUtils` in
//! `api/utils.go` but returns an owning `AppContext` instead of poking
//! package-level globals.
//!
//! Cron format note: gocron's `useSecondLevelCron=false` means a 5-field
//! cron firing at second 0; `tokio_cron_scheduler` requires 6 fields, so
//! we prepend a `"0 "` in that case to keep existing config files
//! working unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};

use crate::api::cache::ApiCache;
use crate::api::limiter::ApiQueryLimiter;
use crate::api::private_lookup::PrivateLookupVerifier;
use crate::api::realtime::RealtimeHub;
use crate::api::state::AppState;
use crate::api::ws_ticket::WsTicketStore;
use crate::config::{Config, RedisConfig, ServerConfig};
use crate::db::engine::{DatabaseEngine, EngineError};
use crate::model::enums::SekaiServerRegion;
use crate::privacy::UidAnonymizer;
use crate::sekai_api::client::{BuildError as SekaiClientError, HarukiSekaiAPIClient};
use crate::tracker::base::TrackerTuning;
use crate::tracker::daemon::{DaemonError, HarukiEventTracker};
use crate::tracker::parser::ParseError;

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("database: {0}")]
    Db(#[from] EngineError),
    #[error("sekai api client: {0}")]
    SekaiClient(#[from] SekaiClientError),
    #[error("tracker: {0}")]
    Tracker(#[from] DaemonError),
    #[error("master data: {0}")]
    MasterData(#[from] ParseError),
    #[error("scheduler: {0}")]
    Scheduler(#[from] JobSchedulerError),
    #[error("privacy config: {0}")]
    Privacy(String),
}

pub struct AppContext {
    pub state: AppState,
    pub dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    pub trackers: HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>>,
    pub scheduler: Option<JobScheduler>,
}

pub async fn build(cfg: &Config) -> Result<AppContext, BootstrapError> {
    let anonymizer = build_anonymizer(cfg)?;
    let private_lookup = PrivateLookupVerifier::from_config(&cfg.toolbox);
    let realtime = RealtimeHub::new();
    let tracker_enabled = cfg
        .servers
        .values()
        .any(|server_cfg| server_cfg.enabled && server_cfg.tracker.enabled);

    let (redis, api) = build_tracker_dependencies(cfg, tracker_enabled).await?;
    let (api_cache, api_cache_redis) = build_api_cache(cfg).await?;

    let mut dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>> = HashMap::new();
    let mut trackers: HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>> = HashMap::new();

    let scheduler = if tracker_enabled {
        Some(JobScheduler::new().await?)
    } else {
        None
    };

    for (server, server_cfg) in &cfg.servers {
        configure_server(
            *server,
            server_cfg,
            &redis,
            &api,
            &api_cache_redis,
            &realtime,
            &anonymizer,
            &scheduler,
            &mut dbs,
            &mut trackers,
        )
        .await?;
    }

    if let Some(scheduler) = &scheduler {
        scheduler.start().await?;
        tracing::info!("scheduler started");
    }

    let query_limiter = ApiQueryLimiter::new(cfg.api_query.clone(), dbs.keys().copied());
    let state = AppState::new(
        dbs.clone(),
        api_cache,
        query_limiter,
        anonymizer,
        private_lookup,
        realtime,
        WsTicketStore::default(),
    );
    Ok(AppContext {
        state,
        dbs,
        trackers,
        scheduler,
    })
}

async fn build_tracker_dependencies(
    cfg: &Config,
    tracker_enabled: bool,
) -> Result<
    (
        Option<redis::aio::ConnectionManager>,
        Option<HarukiSekaiAPIClient>,
    ),
    BootstrapError,
> {
    if !tracker_enabled {
        tracing::info!("all trackers disabled; running API only");
        return Ok((None, None));
    }

    tracing::info!("connecting Redis");
    let client = redis::Client::open(redis_url(&cfg.redis))?;
    let redis = redis::aio::ConnectionManager::new(client).await?;
    tracing::info!("Redis ready");
    let api = HarukiSekaiAPIClient::with_timeouts(
        cfg.sekai_api.api_endpoint.clone(),
        &cfg.sekai_api.api_token,
        std::time::Duration::from_secs(cfg.sekai_api.timeout_secs),
        std::time::Duration::from_secs(cfg.sekai_api.connect_timeout_secs),
    )?;
    Ok((Some(redis), Some(api)))
}

async fn build_api_cache(
    cfg: &Config,
) -> Result<(Option<ApiCache>, Option<redis::aio::ConnectionManager>), BootstrapError> {
    if !cfg.api_cache.enabled {
        return Ok((None, None));
    }
    let redis_url = if cfg.api_cache.redis_url.trim().is_empty() {
        redis_url(&cfg.redis)
    } else {
        cfg.api_cache.redis_url.clone()
    };
    let pool_size = cfg.api_cache.pool_size.max(1);
    tracing::info!(pool_size, "connecting API cache Redis");
    let client = redis::Client::open(redis_url)?;
    let mut conns = Vec::with_capacity(pool_size);
    for _ in 0..pool_size {
        conns.push(redis::aio::ConnectionManager::new(client.clone()).await?);
    }
    let invalidation_conn = redis::aio::ConnectionManager::new(client).await?;
    tracing::info!(pool_size, "API cache Redis ready");
    Ok((
        Some(ApiCache::new(conns, cfg.api_cache.clone())),
        Some(invalidation_conn),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn configure_server(
    server: SekaiServerRegion,
    server_cfg: &ServerConfig,
    redis: &Option<redis::aio::ConnectionManager>,
    api: &Option<HarukiSekaiAPIClient>,
    api_cache_redis: &Option<redis::aio::ConnectionManager>,
    realtime: &RealtimeHub,
    anonymizer: &UidAnonymizer,
    scheduler: &Option<JobScheduler>,
    dbs: &mut HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    trackers: &mut HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>>,
) -> Result<(), BootstrapError> {
    if !server_cfg.enabled {
        tracing::info!(%server, "server disabled, skipping");
        return Ok(());
    }
    tracing::info!(%server, "connecting database");
    let engine = Arc::new(DatabaseEngine::connect(&server_cfg.db).await?);
    dbs.insert(server, engine.clone());

    if !server_cfg.tracker.enabled {
        return Ok(());
    }
    let mut daemon = HarukiEventTracker::new(
        server,
        api.as_ref()
            .expect("sekai api is initialized when any tracker is enabled")
            .clone(),
        redis
            .as_ref()
            .expect("redis is initialized when any tracker is enabled")
            .clone(),
        api_cache_redis.clone(),
        engine,
        realtime.clone(),
        anonymizer.clone(),
        TrackerTuning {
            post_end_user_refresh_interval_secs: server_cfg
                .tracker
                .post_end_user_refresh_interval_secs,
            idle_heartbeat_interval_secs: server_cfg.tracker.idle_heartbeat_interval_secs,
            border_fetch_interval_secs: server_cfg.tracker.border_fetch_interval_secs,
            flush_interval_secs: server_cfg.tracker.flush_interval_secs,
            flush_max_rows: server_cfg.tracker.flush_max_rows.max(1),
            flush_hot_ranks: server_cfg.tracker.flush_hot_ranks,
        },
        &server_cfg.master_data_dir,
    )?;
    if let Err(err) = daemon.init().await {
        tracing::warn!(%server, %err, "tracker init failed; will retry on first tick");
    }
    let daemon = Arc::new(Mutex::new(daemon));
    trackers.insert(server, daemon.clone());

    let cron_expr = scheduler_cron_expr(
        server_cfg.tracker.use_second_level_cron,
        &server_cfg.tracker.cron,
    );
    let daemon_for_job = daemon.clone();
    // At second-level cadence a slow upstream makes skipped ticks routine;
    // rate-limit the warning instead of emitting one per skipped second.
    let last_skip_warn = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let job = Job::new_async(cron_expr.as_str(), move |_uuid, _l| {
        let daemon = daemon_for_job.clone();
        let last_skip_warn = last_skip_warn.clone();
        Box::pin(async move {
            // A tick that outlives the cron interval must not queue the
            // next firing behind the mutex — back-to-back stale ticks
            // would pile onto an already slow upstream/DB.
            match daemon.try_lock() {
                Ok(mut daemon) => {
                    tracing::debug!(%server, "tracker tick");
                    daemon.track_ranking_data().await;
                }
                Err(_) => {
                    let now = chrono::Utc::now().timestamp();
                    let prev = last_skip_warn.load(std::sync::atomic::Ordering::Relaxed);
                    if now - prev >= 60 {
                        last_skip_warn.store(now, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(%server, "previous tracker tick still running; skipping ticks");
                    } else {
                        tracing::debug!(%server, "previous tracker tick still running; skipping this tick");
                    }
                }
            }
        })
    })?;
    scheduler
        .as_ref()
        .expect("scheduler is initialized when any tracker is enabled")
        .add(job)
        .await?;
    tracing::info!(%server, cron = %cron_expr, "scheduled tracker");
    Ok(())
}

/// Normalize the configured cron into the 6-field form
/// `tokio_cron_scheduler` requires. `use_second_level_cron: false` keeps the
/// gocron-era 5-field convention (fires at second 0), but a 6-field
/// expression is accepted as-is either way — blindly prepending `"0 "` to
/// one would produce an unparseable 7-field schedule.
fn scheduler_cron_expr(use_second_level_cron: bool, cron: &str) -> String {
    let fields = cron.split_whitespace().count();
    if use_second_level_cron || fields >= 6 {
        if !use_second_level_cron && fields >= 6 {
            tracing::warn!(
                cron,
                "cron has a seconds field but use_second_level_cron is false; using it as-is"
            );
        }
        cron.to_string()
    } else {
        format!("0 {cron}")
    }
}

fn build_anonymizer(cfg: &Config) -> Result<UidAnonymizer, BootstrapError> {
    let uid = &cfg.privacy.uid_anonymization;
    if !uid.enabled {
        return Ok(UidAnonymizer::disabled());
    }
    if uid.salt.trim().is_empty() {
        return Err(BootstrapError::Privacy(
            "privacy.uid_anonymization.salt is required when enabled".into(),
        ));
    }
    Ok(UidAnonymizer::enabled(uid.salt.clone()))
}

fn redis_url(cfg: &RedisConfig) -> String {
    if cfg.password.is_empty() {
        format!("redis://{}:{}/", cfg.host, cfg.port)
    } else {
        format!("redis://:{}@{}:{}/", cfg.password, cfg.host, cfg.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::db_config::DbConfig;

    fn coverage_redis_config() -> Option<(String, RedisConfig)> {
        let Ok(url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
            return None;
        };
        let authority = url.strip_prefix("redis://")?.trim_end_matches('/');
        let (host, port) = authority.rsplit_once(':')?;
        let host = host.to_owned();
        let port = port.parse().ok()?;
        Some((
            url,
            RedisConfig {
                host,
                port,
                password: String::new(),
            },
        ))
    }

    #[tokio::test]
    async fn builds_api_only_context_with_no_servers() {
        let context = build(&Config::default()).await.unwrap();

        assert!(context.dbs.is_empty());
        assert!(context.trackers.is_empty());
        assert!(context.scheduler.is_none());
    }

    #[tokio::test]
    async fn builds_enabled_api_only_sqlite_server() {
        let mut cfg = Config::default();
        cfg.servers.insert(
            SekaiServerRegion::Jp,
            ServerConfig {
                enabled: true,
                db: DbConfig {
                    dialect: "sqlite".into(),
                    dsn: "sqlite::memory:".into(),
                    max_open_conns: 1,
                    max_idle_conns: 1,
                    ..DbConfig::default()
                },
                ..ServerConfig::default()
            },
        );

        let context = build(&cfg).await.unwrap();

        assert!(context.dbs.contains_key(&SekaiServerRegion::Jp));
        assert!(context.trackers.is_empty());
        assert!(context.scheduler.is_none());
    }

    #[tokio::test]
    async fn skips_disabled_server_configuration() {
        let mut dbs = HashMap::new();
        let mut trackers = HashMap::new();

        configure_server(
            SekaiServerRegion::En,
            &ServerConfig::default(),
            &None,
            &None,
            &None,
            &RealtimeHub::new(),
            &UidAnonymizer::disabled(),
            &None,
            &mut dbs,
            &mut trackers,
        )
        .await
        .unwrap();

        assert!(dbs.is_empty());
        assert!(trackers.is_empty());
    }

    #[tokio::test]
    async fn disabled_api_cache_needs_no_redis() {
        let result = build_api_cache(&Config::default()).await.unwrap();

        assert!(result.0.is_none());
        assert!(result.1.is_none());
    }

    #[tokio::test]
    async fn builds_enabled_cache_and_tracker_dependencies() {
        let Some((url, redis)) = coverage_redis_config() else {
            return;
        };
        let mut cfg = Config {
            redis,
            ..Config::default()
        };
        cfg.api_cache.enabled = true;
        cfg.api_cache.redis_url = url;
        cfg.api_cache.pool_size = 2;
        cfg.sekai_api.api_endpoint = "http://127.0.0.1".into();

        let (cache, invalidation) = build_api_cache(&cfg).await.unwrap();
        assert!(cache.is_some());
        assert!(invalidation.is_some());

        let (redis, api) = build_tracker_dependencies(&cfg, true).await.unwrap();
        assert!(redis.is_some());
        assert!(api.is_some());
    }

    #[test]
    fn builds_anonymizer_from_privacy_configuration() {
        let disabled = build_anonymizer(&Config::default()).unwrap();
        assert!(!disabled.is_enabled());

        let mut enabled_cfg = Config::default();
        enabled_cfg.privacy.uid_anonymization.enabled = true;
        enabled_cfg.privacy.uid_anonymization.salt = "secret".into();
        assert!(build_anonymizer(&enabled_cfg).unwrap().is_enabled());

        enabled_cfg.privacy.uid_anonymization.salt.clear();
        assert!(matches!(
            build_anonymizer(&enabled_cfg),
            Err(BootstrapError::Privacy(_))
        ));
    }

    #[test]
    fn cron_expr_pads_five_fields_and_passes_six_through() {
        assert_eq!(scheduler_cron_expr(false, "*/2 * * * *"), "0 */2 * * * *");
        assert_eq!(scheduler_cron_expr(true, "*/1 * * * * *"), "*/1 * * * * *");
        // 6-field expression with the flag off must not gain a 7th field.
        assert_eq!(scheduler_cron_expr(false, "*/1 * * * * *"), "*/1 * * * * *");
    }

    #[test]
    fn builds_redis_urls_with_and_without_passwords() {
        let mut cfg = RedisConfig {
            host: "redis.internal".into(),
            port: 6380,
            password: String::new(),
        };
        assert_eq!(redis_url(&cfg), "redis://redis.internal:6380/");

        cfg.password = "password".into();
        assert_eq!(redis_url(&cfg), "redis://:password@redis.internal:6380/");
    }
}
