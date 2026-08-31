//! Graceful shutdown order: scheduler → trackers → AppState → DB engines.
//! `DatabaseEngine` (and the underlying sqlx pool) closes its connections
//! via `Drop`, so we don't need exclusive ownership to clean up — letting
//! the last `Arc` go out of scope is enough.
//!
//! Note: `tokio_cron_scheduler::JobScheduler::shutdown()` does *not*
//! release the `Arc` clones it holds inside its job closures, so we
//! cannot rely on `Arc::try_unwrap` to ever succeed for engines that
//! tracker daemons reference. The previous version tried to and always
//! emitted `engine still has live refs` warnings — that path is gone.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_cron_scheduler::JobScheduler;

use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::model::enums::SekaiServerRegion;
use crate::tracker::daemon::HarukiEventTracker;

pub async fn run(
    scheduler: Option<JobScheduler>,
    trackers: HashMap<SekaiServerRegion, Arc<Mutex<HarukiEventTracker>>>,
    dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    state: AppState,
) {
    if let Some(mut scheduler) = scheduler {
        tracing::info!("shutting down scheduler");
        if let Err(err) = scheduler.shutdown().await {
            tracing::error!(%err, "scheduler shutdown failed");
        }
    }
    // The scheduler is stopped, so the lock is at worst held by one
    // still-running tick; waiting on it means the flush sees that tick's
    // samples too.
    for (server, tracker) in &trackers {
        tracing::debug!(%server, "flushing tracker before shutdown");
        tracker.lock().await.flush_on_shutdown().await;
    }
    drop(trackers);
    drop(state);
    drop(dbs);
    tracing::info!("shutdown complete");
}

/// Resolves once SIGINT or SIGTERM is observed (Ctrl+C on Windows).
pub async fn signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::realtime::RealtimeHub;
    use crate::api::ws_ticket::WsTicketStore;
    use crate::config::ApiQueryConfig;
    use crate::privacy::UidAnonymizer;

    #[tokio::test]
    async fn shuts_down_without_scheduler_or_trackers() {
        let state = AppState::new(
            HashMap::new(),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), []),
            UidAnonymizer::disabled(),
            None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        );
        run(None, HashMap::new(), HashMap::new(), state).await;
    }
}
