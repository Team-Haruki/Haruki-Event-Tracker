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
    /// position on the cluster path only. Returns the bumped local cache
    /// epoch — `None` when this process has no API cache to bump.
    pub async fn finish(
        &mut self,
        server: SekaiServerRegion,
        event_id: i64,
        db: &DatabaseEngine,
        message: &str,
    ) -> Option<i64> {
        match self {
            Self::Disabled => None,
            Self::LocalRedis(conn) => match finish_event_update(conn, server, event_id).await {
                Ok(epoch) => Some(epoch),
                Err(err) => {
                    tracing::warn!(%err, "{message}");
                    None
                }
            },
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
                None
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

        assert_eq!(
            CacheInvalidation::from_parts(None, Some(UpdateBus::new()))
                .finish(SekaiServerRegion::Jp, 1, &engine, "x")
                .await,
            None
        );
        let mut disabled = CacheInvalidation::from_parts(None, None);
        assert_eq!(
            disabled
                .finish(SekaiServerRegion::Jp, 1, &engine, "x")
                .await,
            None
        );
        assert!(matches!(disabled, CacheInvalidation::Disabled));
    }

    #[tokio::test]
    async fn local_redis_finish_returns_the_bumped_epoch() {
        let Ok(url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
            return;
        };
        let conn = redis::aio::ConnectionManager::new(redis::Client::open(url).unwrap())
            .await
            .unwrap();
        let engine = DatabaseEngine::from_connection(
            Database::connect("sqlite::memory:").await.unwrap(),
            DatabaseBackend::Sqlite,
        );
        let event_id = chrono::Utc::now().timestamp_micros();
        let mut inv = CacheInvalidation::from_parts(Some(conn), None);
        inv.begin(SekaiServerRegion::Jp, event_id).await;
        let first = inv
            .finish(SekaiServerRegion::Jp, event_id, &engine, "x")
            .await;
        assert_eq!(first, Some(1));
        let second = inv
            .finish(SekaiServerRegion::Jp, event_id, &engine, "x")
            .await;
        assert_eq!(second, Some(2));
    }
}
