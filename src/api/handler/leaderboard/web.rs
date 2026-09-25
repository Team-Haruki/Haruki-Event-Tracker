use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::api::error::ApiError;
use crate::api::handler::leaderboard::service::{
    OverviewPart, OverviewQuery, WebDetailQuery, web_check_room_for_scope, web_overview_for_scope,
    web_overview_part_for_scope, web_rank_detail_for_scope, web_status_for_scope,
    web_user_detail_for_scope,
};
use crate::api::handler::web::UserSearchQuery;
use crate::api::http_cache;
use crate::api::json::{EncodedJson, Json, RawJson, accepts_gzip};
use crate::api::state::AppState;
use crate::model::api::WebRankDetailResponseSchema;

#[tracing::instrument(skip(state, query, headers), fields(server, event_id))]
pub async fn total_overview(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(state, server, event_id, None, query, "web:v2", prefer_gzip).await
}

#[tracing::instrument(skip(state, query, headers), fields(server, event_id, character_id))]
pub async fn world_bloom_overview(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        "web:v2",
        prefer_gzip,
    )
    .await
}

#[tracing::instrument(skip(state, query, headers), fields(server, event_id))]
pub async fn total_replay_overview(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(
        state,
        server,
        event_id,
        None,
        query,
        "web:v2:replay",
        prefer_gzip,
    )
    .await
}

#[tracing::instrument(skip(state, query, headers), fields(server, event_id, character_id))]
pub async fn world_bloom_replay_overview(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<OverviewQuery>,
    headers: HeaderMap,
) -> Result<EncodedJson, ApiError> {
    let prefer_gzip = accepts_gzip(&headers);
    web_overview_for_scope(
        state,
        server,
        event_id,
        Some(character_id),
        query,
        "web:v2:replay",
        prefer_gzip,
    )
    .await
}

macro_rules! overview_part_handlers {
    ($($total:ident, $world_bloom:ident => $part:expr;)*) => {$(
        #[tracing::instrument(skip(state, query, headers), fields(server, event_id))]
        pub async fn $total(
            State(state): State<AppState>,
            Path((server, event_id)): Path<(String, i64)>,
            Query(query): Query<OverviewQuery>,
            headers: HeaderMap,
        ) -> Result<EncodedJson, ApiError> {
            let prefer_gzip = accepts_gzip(&headers);
            web_overview_part_for_scope(state, server, event_id, None, $part, query, prefer_gzip)
                .await
        }

        #[tracing::instrument(skip(state, query, headers), fields(server, event_id, character_id))]
        pub async fn $world_bloom(
            State(state): State<AppState>,
            Path((server, event_id, character_id)): Path<(String, i64, i64)>,
            Query(query): Query<OverviewQuery>,
            headers: HeaderMap,
        ) -> Result<EncodedJson, ApiError> {
            let prefer_gzip = accepts_gzip(&headers);
            web_overview_part_for_scope(
                state,
                server,
                event_id,
                Some(character_id),
                $part,
                query,
                prefer_gzip,
            )
            .await
        }
    )*};
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn total_status(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<OverviewQuery>,
) -> Result<Response, ApiError> {
    Ok(web_status_for_scope(state, server, event_id, None, query)
        .await?
        .into_response())
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_status(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<OverviewQuery>,
) -> Result<Response, ApiError> {
    Ok(
        web_status_for_scope(state, server, event_id, Some(character_id), query)
            .await?
            .into_response(),
    )
}

overview_part_handlers! {
    total_top100, world_bloom_top100 => OverviewPart::Top100;
    total_borders, world_bloom_borders => OverviewPart::Borders;
    total_growth, world_bloom_growth => OverviewPart::Growth;
}

#[tracing::instrument(skip(state, query), fields(server, event_id, rank))]
pub async fn total_rank_detail(
    State(state): State<AppState>,
    Path((server, event_id, rank)): Path<(String, i64, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebRankDetailResponseSchema>, ApiError> {
    web_rank_detail_for_scope(state, server, event_id, None, rank, query).await
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id, rank))]
pub async fn world_bloom_rank_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, rank)): Path<(String, i64, i64, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Json<WebRankDetailResponseSchema>, ApiError> {
    web_rank_detail_for_scope(state, server, event_id, Some(character_id), rank, query).await
}

#[tracing::instrument(skip(state, query, user_id), fields(server, event_id))]
pub async fn total_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Response, ApiError> {
    user_detail(state, server, event_id, None, user_id, query).await
}

#[tracing::instrument(skip(state, query, user_id), fields(server, event_id, character_id))]
pub async fn world_bloom_user_detail(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Response, ApiError> {
    user_detail(state, server, event_id, Some(character_id), user_id, query).await
}

/// A raw-UID lookup answers with that UID, so it is kept out of shared
/// caches; a `unique_id` lookup is ordinary public data.
async fn user_detail(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    user_id: String,
    query: WebDetailQuery,
) -> Result<Response, ApiError> {
    let raw = query.looks_up_raw_uid(&user_id)?;
    let detail =
        web_user_detail_for_scope(state, server, event_id, character_id, user_id, query).await?;
    Ok(if raw {
        http_cache::private(detail)
    } else {
        detail.into_response()
    })
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn total_check_room(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Response, ApiError> {
    web_check_room_for_scope(state, server, event_id, None, query)
        .await
        .map(http_cache::private)
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_check_room(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<WebDetailQuery>,
) -> Result<Response, ApiError> {
    web_check_room_for_scope(state, server, event_id, Some(character_id), query)
        .await
        .map(http_cache::private)
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_users(
    State(state): State<AppState>,
    Path((server, event_id, _character_id)): Path<(String, i64, i64)>,
    Query(query): Query<UserSearchQuery>,
) -> Result<RawJson, ApiError> {
    crate::api::handler::web::users(State(state), Path((server, event_id)), Query(query)).await
}
