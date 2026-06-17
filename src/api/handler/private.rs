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
use crate::db::query::ranking::{
    fetch_all_rankings, fetch_latest_ranking, fetch_latest_ranking_by_rank,
};
use crate::db::query::user::get_user_data;
use crate::db::query::world_bloom::{
    fetch_all_world_bloom_rankings, fetch_latest_world_bloom_ranking,
    fetch_latest_world_bloom_ranking_by_rank,
};
use crate::model::api::{
    LeaderboardMetaSchema, RecordedRankData, UserAllRankingDataQueryResponseSchema,
    UserLatestRankingQueryResponseSchema, WebRankingItemSchema, WebUserDetailResponseSchema,
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

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivateWebDetailQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
    include_trace: Option<bool>,
    include_profile: Option<bool>,
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
    owner: Option<&str>,
    server: SekaiServerRegion,
    user_id: &str,
) -> Result<(), ApiError> {
    let verifier = state.private_lookup().ok_or_else(|| {
        ApiError::ServiceUnavailable("private lookup verifier is not configured".into())
    })?;
    verifier
        .verify_bound_user(&subject.0, owner, server, user_id)
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
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
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
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
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
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let _permit = state.query_limiter().acquire_trace(region).await?;
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
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let _permit = state.query_limiter().acquire_trace(region).await?;
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

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn web_total_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<PrivateWebDetailQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(state, server, event_id, None, user_id, query, subject).await
}

#[tracing::instrument(skip(state), fields(server, event_id, character_id, user_id))]
pub async fn web_world_bloom_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<PrivateWebDetailQuery>,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    web_user_detail_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        user_id,
        query,
        subject,
    )
    .await
}

async fn web_user_detail_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    user_id: String,
    query: PrivateWebDetailQuery,
    subject: axum::Extension<PrivateSubject>,
) -> Result<Json<WebUserDetailResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    require_bound_user(
        &state,
        &subject,
        query.owner.as_deref().or(query.owner_id.as_deref()),
        region,
        &user_id,
    )
    .await?;
    let mode = prepare_private_user_id_mode(&state, &engine, region, event_id).await?;
    let current = match character_id {
        Some(character_id) => {
            fetch_latest_world_bloom_ranking(&engine, event_id, &user_id, character_id, mode)
                .await?
                .map(RecordedRankData::WorldBloom)
        }
        None => fetch_latest_ranking(&engine, event_id, &user_id, mode)
            .await?
            .map(RecordedRankData::Normal),
    };
    let rank = current.as_ref().map(rank_of_rank_data);
    let previous = if let Some(rank) = rank.filter(|rank| *rank > 1) {
        fetch_rank_item(&engine, event_id, character_id, rank - 1, mode).await?
    } else {
        None
    };
    let next = if let Some(rank) = rank {
        fetch_rank_item(&engine, event_id, character_id, rank + 1, mode).await?
    } else {
        None
    };
    let current = current.map(|rank_data| WebRankingItemSchema {
        rank_data,
        user_data: None,
    });
    let player_trace = if query.include_trace.unwrap_or(false) {
        let _permit = state.query_limiter().acquire_trace(region).await?;
        match character_id {
            Some(character_id) => {
                fetch_all_world_bloom_rankings(&engine, event_id, &user_id, character_id, mode)
                    .await?
                    .into_iter()
                    .map(RecordedRankData::WorldBloom)
                    .collect()
            }
            None => fetch_all_rankings(&engine, event_id, &user_id, mode)
                .await?
                .into_iter()
                .map(RecordedRankData::Normal)
                .collect(),
        }
    } else {
        Vec::new()
    };
    let profile = if query.include_profile.unwrap_or(false) {
        get_user_data(&engine, event_id, &user_id, mode).await?
    } else {
        None
    };
    if current.is_none() && player_trace.is_empty() && profile.is_none() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(WebUserDetailResponseSchema {
        meta: LeaderboardMetaSchema {
            server,
            event_id,
            scope: match character_id {
                Some(character_id) => format!("world-bloom/{character_id}"),
                None => "total".to_owned(),
            },
            character_id,
            fetched_at: chrono::Utc::now().timestamp(),
        },
        current,
        previous,
        next,
        player_trace,
        profile,
    }))
}

async fn fetch_rank_item(
    engine: &crate::db::engine::DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    rank: i64,
    mode: crate::db::query::user::PublicUserIdMode,
) -> Result<Option<WebRankingItemSchema>, ApiError> {
    let rank_data = match character_id {
        Some(character_id) => {
            fetch_latest_world_bloom_ranking_by_rank(engine, event_id, rank, character_id, mode)
                .await?
                .map(RecordedRankData::WorldBloom)
        }
        None => fetch_latest_ranking_by_rank(engine, event_id, rank, mode)
            .await?
            .map(RecordedRankData::Normal),
    };
    Ok(rank_data.map(|rank_data| WebRankingItemSchema {
        rank_data,
        user_data: None,
    }))
}

fn rank_of_rank_data(rank_data: &RecordedRankData) -> i64 {
    match rank_data {
        RecordedRankData::Normal(data) => data.rank,
        RecordedRankData::WorldBloom(data) => data.rank,
    }
}
