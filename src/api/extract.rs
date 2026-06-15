//! Helpers shared across handlers. Each route uses Axum `Path<(...)>`
//! tuples directly, but the `:server` segment always needs the same
//! parse-and-lookup dance — wrapped here as `resolve_engine`.

use std::sync::Arc;

use crate::api::error::ApiError;
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::db::query::user::PublicUserIdMode;
use crate::model::enums::SekaiServerRegion;

const MAX_BATCH_RANKS: usize = 100;

pub fn resolve_engine(state: &AppState, server: &str) -> Result<Arc<DatabaseEngine>, ApiError> {
    let (_, engine) = resolve_region_engine(state, server)?;
    Ok(engine)
}

pub fn resolve_region_engine(
    state: &AppState,
    server: &str,
) -> Result<(SekaiServerRegion, Arc<DatabaseEngine>), ApiError> {
    let region = SekaiServerRegion::parse(server)
        .ok_or_else(|| ApiError::InvalidServer(server.to_owned()))?;
    state
        .db(region)
        .cloned()
        .map(|engine| (region, engine))
        .ok_or_else(|| ApiError::InvalidServer(server.to_owned()))
}

pub async fn prepare_user_id_mode(
    state: &AppState,
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
) -> Result<PublicUserIdMode, ApiError> {
    state
        .ensure_user_table_extensions(engine, server, event_id)
        .await?;
    if state.anonymizer().is_enabled() {
        Ok(PublicUserIdMode::Unique)
    } else {
        Ok(PublicUserIdMode::Raw)
    }
}

pub async fn prepare_private_user_id_mode(
    state: &AppState,
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
) -> Result<PublicUserIdMode, ApiError> {
    state
        .ensure_user_table_extensions(engine, server, event_id)
        .await?;
    Ok(PublicUserIdMode::Raw)
}

pub fn parse_rank_query(raw: Option<&str>) -> Result<Vec<i64>, ApiError> {
    let Some(raw) = raw else {
        return Err(ApiError::BadRequest("rank query is required".into()));
    };

    let mut ranks = Vec::new();
    for pair in raw.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "rank" && key != "ranks" {
            continue;
        }
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let rank = part
                .parse::<i64>()
                .map_err(|_| ApiError::BadRequest(format!("invalid rank: {part}")))?;
            if rank <= 0 {
                return Err(ApiError::BadRequest(format!("invalid rank: {rank}")));
            }
            if !ranks.contains(&rank) {
                ranks.push(rank);
            }
        }
    }

    if ranks.is_empty() {
        return Err(ApiError::BadRequest("rank query is required".into()));
    }
    if ranks.len() > MAX_BATCH_RANKS {
        return Err(ApiError::BadRequest(format!(
            "too many ranks: max {MAX_BATCH_RANKS}"
        )));
    }
    Ok(ranks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repeated_and_csv_rank_query() {
        assert_eq!(
            parse_rank_query(Some("rank=1&rank=100&ranks=500,100")).unwrap(),
            vec![1, 100, 500]
        );
    }

    #[test]
    fn rejects_empty_or_invalid_rank_query() {
        assert!(parse_rank_query(None).is_err());
        assert!(parse_rank_query(Some("foo=1")).is_err());
        assert!(parse_rank_query(Some("rank=0")).is_err());
        assert!(parse_rank_query(Some("rank=abc")).is_err());
    }
}
