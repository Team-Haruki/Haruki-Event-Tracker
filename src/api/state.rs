//! Shared application state passed to every Axum handler.
//!
//! Holds one `DatabaseEngine` per enabled server, keyed by
//! `SekaiServerRegion`. Wrapped in `Arc` so cloning the state for each
//! request is cheap. Mirrors the `sekaiDBs` map that `api/utils.go`
//! exposed as a package-level singleton in Go.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::api::cache::ApiCache;
use crate::api::limiter::ApiQueryLimiter;
use crate::api::private_lookup::PrivateLookupVerifier;
use crate::api::realtime::RealtimeHub;
use crate::api::ws_ticket::WsTicketStore;
use crate::db::engine::DatabaseEngine;
use crate::db::privacy::ensure_user_table_extensions;
use crate::model::enums::SekaiServerRegion;
use crate::privacy::UidAnonymizer;
use sea_orm::DbErr;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    cache: Option<ApiCache>,
    query_limiter: ApiQueryLimiter,
    anonymizer: UidAnonymizer,
    user_table_extension_cache: Mutex<HashSet<UserTableExtensionKey>>,
    private_lookup: Option<PrivateLookupVerifier>,
    realtime: RealtimeHub,
    ws_tickets: WsTicketStore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UserTableExtensionKey {
    server: SekaiServerRegion,
    event_id: i64,
    anonymization_enabled: bool,
}

impl AppState {
    pub fn new(
        dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
        cache: Option<ApiCache>,
        query_limiter: ApiQueryLimiter,
        anonymizer: UidAnonymizer,
        private_lookup: Option<PrivateLookupVerifier>,
        realtime: RealtimeHub,
        ws_tickets: WsTicketStore,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                dbs,
                cache,
                query_limiter,
                anonymizer,
                user_table_extension_cache: Mutex::new(HashSet::new()),
                private_lookup,
                realtime,
                ws_tickets,
            }),
        }
    }

    pub fn db(&self, server: SekaiServerRegion) -> Option<&Arc<DatabaseEngine>> {
        self.inner.dbs.get(&server)
    }

    pub fn dbs(&self) -> impl Iterator<Item = (SekaiServerRegion, Arc<DatabaseEngine>)> + '_ {
        self.inner
            .dbs
            .iter()
            .map(|(&server, db)| (server, db.clone()))
    }

    pub fn cache(&self) -> Option<&ApiCache> {
        self.inner.cache.as_ref()
    }

    pub fn query_limiter(&self) -> &ApiQueryLimiter {
        &self.inner.query_limiter
    }

    pub fn anonymizer(&self) -> &UidAnonymizer {
        &self.inner.anonymizer
    }

    pub async fn ensure_user_table_extensions(
        &self,
        engine: &DatabaseEngine,
        server: SekaiServerRegion,
        event_id: i64,
    ) -> Result<(), DbErr> {
        let key = UserTableExtensionKey {
            server,
            event_id,
            anonymization_enabled: self.inner.anonymizer.is_enabled(),
        };
        let mut cache = self.inner.user_table_extension_cache.lock().await;
        if cache.contains(&key) {
            return Ok(());
        }
        ensure_user_table_extensions(engine, server, event_id, &self.inner.anonymizer).await?;
        cache.insert(key);
        Ok(())
    }

    pub fn private_lookup(&self) -> Option<&PrivateLookupVerifier> {
        self.inner.private_lookup.as_ref()
    }

    pub fn realtime(&self) -> &RealtimeHub {
        &self.inner.realtime
    }

    pub fn ws_tickets(&self) -> &WsTicketStore {
        &self.inner.ws_tickets
    }
}
