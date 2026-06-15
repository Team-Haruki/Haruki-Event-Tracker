//! Login-protected raw UID lookup endpoints for Toolbox-bound accounts.
//!
//! Public ranking/user endpoints switch to anonymized unique IDs when privacy
//! mode is enabled. These private endpoints keep raw UID lookup available only
//! when the trusted Oathkeeper/WebSocket subject owns the requested Toolbox game
//! account binding.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::api::access_log::ProxyTrust;
use crate::api::error::ApiError;
use crate::api::extract::{prepare_private_user_id_mode, resolve_region_engine};
use crate::api::json::Json;
use crate::api::private_lookup::PrivateLookupError;
use crate::api::state::AppState;
use crate::api::ws_ticket::{resolve_trusted_subject, unauthorized};
use crate::db::query::ranking::{fetch_all_rankings, fetch_latest_ranking};
use crate::db::query::user::get_user_data;
use crate::db::query::world_bloom::{
    fetch_all_world_bloom_rankings, fetch_latest_world_bloom_ranking,
};
use crate::model::api::{
    RecordedRankData, UserAllRankingDataQueryResponseSchema, UserLatestRankingQueryResponseSchema,
};
use crate::model::enums::SekaiServerRegion;

#[derive(Debug, Clone)]
pub struct PrivateSubject(pub String);

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivateLookupQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
}

pub async fn require_subject(
    State(trust): State<Arc<ProxyTrust>>,
    mut req: Request,
    next: Next,
) -> Response {
    let subject = req
        .extensions()
        .get::<PrivateSubject>()
        .map(|subject| subject.0.clone())
        .or_else(|| {
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr);
            resolve_trusted_subject(req.headers(), &trust, peer)
        });
    let Some(subject) = subject else {
        return unauthorized().into_response();
    };

    req.extensions_mut().insert(PrivateSubject(subject));
    next.run(req).await
}

async fn require_bound_user(
    state: &AppState,
    subject: &PrivateSubject,
    query: &PrivateLookupQuery,
    server: SekaiServerRegion,
    user_id: &str,
) -> Result<(), ApiError> {
    let verifier = state.private_lookup().ok_or_else(|| {
        ApiError::ServiceUnavailable("private lookup verifier is not configured".into())
    })?;
    verifier
        .verify_bound_user(
            &subject.0,
            query.owner.as_deref().or(query.owner_id.as_deref()),
            server,
            user_id,
        )
        .await
        .map_err(map_private_lookup_error)
}

fn map_private_lookup_error(err: PrivateLookupError) -> ApiError {
    match err {
        PrivateLookupError::NotConfigured => {
            ApiError::ServiceUnavailable("private lookup verifier is not configured".into())
        }
        PrivateLookupError::Unauthorized => ApiError::Unauthorized,
        PrivateLookupError::Forbidden => ApiError::Forbidden,
        PrivateLookupError::Upstream => {
            ApiError::ServiceUnavailable("private lookup verifier request failed".into())
        }
    }
}

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn latest_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(&state, &subject, &query, region, &user_id).await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let ranking = fetch_latest_ranking(&engine, event_id, &user_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if ranking.is_none() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserLatestRankingQueryResponseSchema {
        rank_data: ranking.map(RecordedRankData::Normal),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn latest_world_bloom_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserLatestRankingQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(&state, &subject, &query, region, &user_id).await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let ranking =
        fetch_latest_world_bloom_ranking(&engine, event_id, &user_id, character_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if ranking.is_none() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserLatestRankingQueryResponseSchema {
        rank_data: ranking.map(RecordedRankData::WorldBloom),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn trace_by_user(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(&state, &subject, &query, region, &user_id).await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let rankings = fetch_all_rankings(&engine, event_id, &user_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if rankings.is_empty() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings.into_iter().map(RecordedRankData::Normal).collect(),
        user_data,
    }))
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn trace_world_bloom_by_user(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<PrivateLookupQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(&state, &subject, &query, region, &user_id).await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let rankings =
        fetch_all_world_bloom_rankings(&engine, event_id, &user_id, character_id, mode).await?;
    let user_data = get_user_data(&engine, event_id, &user_id, mode)
        .await
        .ok()
        .flatten();
    if rankings.is_empty() && user_data.is_none() {
        return Err(ApiError::NotFound);
    }

    Ok(Json(UserAllRankingDataQueryResponseSchema {
        rank_data: rankings
            .into_iter()
            .map(RecordedRankData::WorldBloom)
            .collect(),
        user_data,
    }))
}
