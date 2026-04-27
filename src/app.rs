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

use crate::api::state::AppState;
use crate::config::Config;
use crate::db::engine::{DatabaseEngine, EngineError};
use crate::model::enums::SekaiServerRegion;
use crate::sekai_api::client::{BuildError as SekaiClientError, HarukiSekaiAPIClient};
use crate::tracker::daemon::HarukiEventTracker;

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("database: {0}")]
    Db(#[from] EngineError),
    #[error("sekai api client: {0}")]
    SekaiClient(#[from] SekaiClientError),
    #[error("scheduler: {0}")]
    Scheduler(#[from] JobSchedulerError),
}

pub struct AppContext {
    pub state: AppState,
    pub dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    pub trackers: HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>>,
    pub scheduler: JobScheduler,
}

pub async fn build(cfg: &Config) -> Result<AppContext, BootstrapError> {
    tracing::info!("connecting Redis");
    let redis_url = if cfg.redis.password.is_empty() {
        format!("redis://{}:{}/", cfg.redis.host, cfg.redis.port)
    } else {
        format!(
            "redis://:{}@{}:{}/",
            cfg.redis.password, cfg.redis.host, cfg.redis.port
        )
    };
    let client = redis::Client::open(redis_url)?;
    let redis = redis::aio::ConnectionManager::new(client).await?;
    tracing::info!("Redis ready");

    let api = HarukiSekaiAPIClient::new(
        cfg.sekai_api.api_endpoint.clone(),
        &cfg.sekai_api.api_token,
    )?;

    let mut dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>> = HashMap::new();
    let mut trackers: HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>> = HashMap::new();

    let scheduler = JobScheduler::new().await?;

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
        let mut daemon = HarukiEventTracker::new(
            *server,
            api.clone(),
            redis.clone(),
            engine.clone(),
            &server_cfg.master_data_dir,
        );
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
        scheduler.add(job).await?;
        tracing::info!(%server, cron = %cron_expr, "scheduled tracker");
    }

    scheduler.start().await?;
    tracing::info!("scheduler started");

    let state = AppState::new(dbs.clone());
    Ok(AppContext {
        state,
        dbs,
        trackers,
        scheduler,
    })
}
