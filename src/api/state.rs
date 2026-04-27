//! Shared application state passed to every Axum handler.
//!
//! Holds one `DatabaseEngine` per enabled server, keyed by
//! `SekaiServerRegion`. Wrapped in `Arc` so cloning the state for each
//! request is cheap. Mirrors the `sekaiDBs` map that `api/utils.go`
//! exposed as a package-level singleton in Go.

use std::collections::HashMap;
use std::sync::Arc;

use crate::db::engine::DatabaseEngine;
use crate::model::enums::SekaiServerRegion;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
}

impl AppState {
    pub fn new(dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>) -> Self {
        Self {
            inner: Arc::new(Inner { dbs }),
        }
    }

    pub fn db(&self, server: SekaiServerRegion) -> Option<&Arc<DatabaseEngine>> {
        self.inner.dbs.get(&server)
    }
}
