//! Shared application state passed to every Axum handler.
//!
//! Holds one `DatabaseEngine` per enabled server, keyed by
//! `SekaiServerRegion`. Wrapped in `Arc` so cloning the state for each
//! request is cheap. Mirrors the `sekaiDBs` map that `api/utils.go`
//! exposed as a package-level singleton in Go.

use std::collections::HashMap;
use std::sync::Arc;

use crate::api::cache::ApiCache;
use crate::db::engine::DatabaseEngine;
use crate::model::enums::SekaiServerRegion;
use crate::privacy::UidAnonymizer;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    cache: Option<ApiCache>,
    anonymizer: UidAnonymizer,
}

impl AppState {
    pub fn new(
        dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
        cache: Option<ApiCache>,
        anonymizer: UidAnonymizer,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                dbs,
                cache,
                anonymizer,
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

    pub fn anonymizer(&self) -> &UidAnonymizer {
        &self.inner.anonymizer
    }
}
