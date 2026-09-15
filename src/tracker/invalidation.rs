//! How a tracker tells API caches that an event's data changed.
//!
//! `standalone` keeps the pre-cluster behaviour: dirty flag + epoch bump on
//! the Redis the API layer reads. A cluster `writer` has no local API, so
//! it publishes to the `UpdateBus` instead; readers apply the epoch bump on
//! their own Redis when the event arrives. The dirty flag has no remote
//! equivalent — readers only ever learn about *committed* data.

use redis::aio::ConnectionManager;

use crate::api::cache::{abort_event_update, begin_event_update, finish_event_update};
use crate::cluster::UpdateBus;
use crate::db::engine::DatabaseEngine;
use crate::db::replication::current_wal_lsn;
use crate::model::enums::SekaiServerRegion;

#[derive(Clone)]
pub enum CacheInvalidation {
    Disabled,
    LocalRedis(ConnectionManager),
    Cluster(UpdateBus),
}

impl CacheInvalidation {
    pub fn from_parts(api_cache_redis: Option<ConnectionManager>, bus: Option<UpdateBus>) -> Self {
        match (bus, api_cache_redis) {
            (Some(bus), _) => Self::Cluster(bus),
            (None, Some(conn)) => Self::LocalRedis(conn),
            (None, None) => Self::Disabled,
        }
    }

    pub async fn begin(&mut self, server: SekaiServerRegion, event_id: i64) {
        if let Self::LocalRedis(conn) = self
            && let Err(err) = begin_event_update(conn, server, event_id).await
        {
            tracing::warn!(%err, "failed to mark API cache dirty");
        }
    }

    pub async fn abort(&mut self, server: SekaiServerRegion, event_id: i64, message: &str) {
        if let Self::LocalRedis(conn) = self
            && let Err(err) = abort_event_update(conn, server, event_id).await
        {
            tracing::warn!(%err, "{message}");
        }
    }

    /// Called after the data is committed. `db` is consulted for the WAL
    /// position on the cluster path only.
    pub async fn finish(
        &mut self,
        server: SekaiServerRegion,
        event_id: i64,
        db: &DatabaseEngine,
        message: &str,
    ) {
        match self {
            Self::Disabled => {}
            Self::LocalRedis(conn) => {
                if let Err(err) = finish_event_update(conn, server, event_id).await {
                    tracing::warn!(%err, "{message}");
                }
            }
            Self::Cluster(bus) => {
                let lsn = match current_wal_lsn(db).await {
                    Ok(lsn) => lsn,
                    Err(err) => {
                        tracing::warn!(%err, "failed to read WAL position; publishing without lsn");
                        None
                    }
                };
                let seq = bus.publish(server, event_id, chrono::Utc::now().timestamp(), lsn);
                tracing::debug!(%server, event_id, seq, "published cluster update");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{Database, DatabaseBackend};

    #[tokio::test]
    async fn cluster_mode_publishes_on_finish_only() {
        let bus = UpdateBus::new();
        let mut rx = bus.subscribe();
        let mut inv = CacheInvalidation::from_parts(None, Some(bus.clone()));
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);

        inv.begin(SekaiServerRegion::Jp, 1).await;
        inv.abort(SekaiServerRegion::Jp, 1, "x").await;
        assert!(rx.try_recv().is_err());
        inv.finish(SekaiServerRegion::Jp, 1, &engine, "x").await;
        let event = rx.try_recv().unwrap();
        assert_eq!(
            (event.server, event.event_id, event.seq),
            (SekaiServerRegion::Jp, 1, 1)
        );
        assert!(event.lsn.is_none());

        let mut disabled = CacheInvalidation::from_parts(None, None);
        disabled
            .finish(SekaiServerRegion::Jp, 1, &engine, "x")
            .await;
        assert!(matches!(disabled, CacheInvalidation::Disabled));
    }
}
