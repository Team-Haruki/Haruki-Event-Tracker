//! Graceful shutdown. Mirrors the order in `api.Shutdown`:
//! scheduler → trackers (via Drop) → DB engines. Redis closes when its
//! `ConnectionManager` is dropped along with the trackers.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_cron_scheduler::JobScheduler;

use crate::db::engine::DatabaseEngine;
use crate::model::enums::SekaiServerRegion;
use crate::tracker::daemon::HarukiEventTracker;

pub async fn run(
    mut scheduler: JobScheduler,
    trackers: HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>>,
    dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
) {
    tracing::info!("shutting down scheduler");
    if let Err(err) = scheduler.shutdown().await {
        tracing::error!(%err, "scheduler shutdown failed");
    }

    drop(trackers);

    for (server, engine) in dbs {
        match Arc::try_unwrap(engine) {
            Ok(engine) => {
                if let Err(err) = engine.close().await {
                    tracing::error!(%server, %err, "db close failed");
                }
            }
            Err(_) => {
                tracing::warn!(%server, "db engine still has live refs; skipping close");
            }
        }
    }
    tracing::info!("shutdown complete");
}

/// Resolves once SIGINT or SIGTERM is observed (Ctrl+C on Windows).
pub async fn signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint =
            signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => tracing::info!("SIGINT received"),
            _ = sigterm.recv() => tracing::info!("SIGTERM received"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Ctrl+C received");
    }
}
