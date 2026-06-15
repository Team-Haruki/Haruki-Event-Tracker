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
use crate::config::{Config, RedisConfig};
use crate::db::engine::{DatabaseEngine, EngineError};
use crate::model::enums::SekaiServerRegion;
use crate::privacy::UidAnonymizer;
use crate::sekai_api::client::{BuildError as SekaiClientError, HarukiSekaiAPIClient};
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

    let (redis, api) = if tracker_enabled {
        tracing::info!("connecting Redis");
        let redis_url = redis_url(&cfg.redis);
        let client = redis::Client::open(redis_url)?;
        let redis = redis::aio::ConnectionManager::new(client).await?;
        tracing::info!("Redis ready");

        let api = HarukiSekaiAPIClient::new(
            cfg.sekai_api.api_endpoint.clone(),
            &cfg.sekai_api.api_token,
        )?;
        (Some(redis), Some(api))
    } else {
        tracing::info!("all trackers disabled; running API only");
        (None, None)
    };

    let (api_cache, api_cache_redis) = if cfg.api_cache.enabled {
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
        (
            Some(ApiCache::new(conns, cfg.api_cache.clone())),
            Some(invalidation_conn),
        )
    } else {
        (None, None)
    };

    let mut dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>> = HashMap::new();
    let mut trackers: HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>> = HashMap::new();

    let scheduler = if tracker_enabled {
        Some(JobScheduler::new().await?)
    } else {
        None
    };

    for (server, server_cfg) in &cfg.servers {
        if !server_cfg.enabled {
            tracing::info!(%server, "server disabled, skipping");
            continue;
        }
        tracing::info!(%server, "connecting database");
        let engine = Arc::new(DatabaseEngine::connect(&server_cfg.db).await?);
        dbs.insert(*server, engine.clone());

        if !server_cfg.tracker.enabled {
            continue;
        }
        let redis = redis
            .as_ref()
            .expect("redis is initialized when any tracker is enabled")
            .clone();
        let api = api
            .as_ref()
            .expect("sekai api is initialized when any tracker is enabled")
            .clone();
        let mut daemon = HarukiEventTracker::new(
            *server,
            api,
            redis,
            api_cache_redis.clone(),
            engine.clone(),
            realtime.clone(),
            anonymizer.clone(),
            server_cfg.tracker.post_end_user_refresh_interval_secs,
            &server_cfg.master_data_dir,
        )?;
        if let Err(err) = daemon.init().await {
            tracing::warn!(%server, %err, "tracker init failed; will retry on first tick");
        }
        let daemon = Arc::new(Mutex::new(daemon));
        trackers.insert(*server, daemon.clone());

        let cron_expr = if server_cfg.tracker.use_second_level_cron {
            server_cfg.tracker.cron.clone()
        } else {
            format!("0 {}", server_cfg.tracker.cron)
        };
        let server_label = *server;
        let daemon_for_job = daemon.clone();
        let job = Job::new_async(cron_expr.as_str(), move |_uuid, _l| {
            let daemon = daemon_for_job.clone();
            Box::pin(async move {
                tracing::info!(server = %server_label, "tracker tick");
                daemon.lock().await.track_ranking_data().await;
            })
        })?;
        scheduler
            .as_ref()
            .expect("scheduler is initialized when any tracker is enabled")
            .add(job)
            .await?;
        tracing::info!(%server, cron = %cron_expr, "scheduled tracker");
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
