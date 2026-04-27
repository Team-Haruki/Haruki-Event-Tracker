//! Helpers shared across handlers. Each route uses Axum `Path<(...)>`
//! tuples directly, but the `:server` segment always needs the same
//! parse-and-lookup dance — wrapped here as `resolve_engine`.

use std::sync::Arc;

use crate::api::error::ApiError;
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::model::enums::SekaiServerRegion;

pub fn resolve_engine(state: &AppState, server: &str) -> Result<Arc<DatabaseEngine>, ApiError> {
    let region = SekaiServerRegion::parse(server)
        .ok_or_else(|| ApiError::InvalidServer(server.to_owned()))?;
    state
        .db(region)
        .cloned()
        .ok_or_else(|| ApiError::InvalidServer(server.to_owned()))
}
