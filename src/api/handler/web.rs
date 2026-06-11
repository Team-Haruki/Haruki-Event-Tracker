use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::api::cache::CacheTtl;
use crate::api::error::ApiError;
use crate::api::extract::resolve_region_engine;
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::db::privacy::ensure_user_table_extensions;
use crate::db::query::user::PublicUserIdMode;
use crate::db::query::web::{
    WebRankingCursor, WebRankingFilter, WebTraceFilter, WebUserSearchFilter, search_rankings,
    search_user_trace, search_users, search_world_bloom_rankings, search_world_bloom_user_trace,
};
use crate::model::api::{
    RecordedRankData, UserAllRankingDataQueryResponseSchema, WebRankingPageSchema,
    WebUserSearchPageSchema,
};
use crate::model::enums::SekaiServerRegion;

const DEFAULT_PAGE_LIMIT: u64 = 100;
const MAX_PAGE_LIMIT: u64 = 500;
const DEFAULT_TRACE_LIMIT: u64 = 500;
const MAX_TRACE_LIMIT: u64 = 5000;
const MIN_SEARCH_LEN: usize = 2;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankingSearchQuery {
    rank_min: Option<i64>,
    rank_max: Option<i64>,
    score_min: Option<i64>,
    score_max: Option<i64>,
    start_time: Option<i64>,
    end_time: Option<i64>,
    before: Option<i64>,
    after: Option<i64>,
    timestamp: Option<i64>,
    cursor: Option<String>,
    limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTraceQuery {
    start_time: Option<i64>,
    end_time: Option<i64>,
    cursor: Option<i64>,
    limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSearchQuery {
    unique_id: Option<String>,
    name: Option<String>,
    profile_word: Option<String>,
    card_id: Option<i64>,
    card_level: Option<i64>,
    card_master_rank: Option<i64>,
    card_special_training_status: Option<String>,
    card_default_image: Option<String>,
    cheerful_team_id: Option<i64>,
    cursor: Option<i64>,
    limit: Option<u64>,
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn rankings(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<RankingSearchQuery>,
) -> Result<Json<WebRankingPageSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_web_user_id_mode(&state, &engine, region, event_id).await?;
    let filter = query.into_filter()?;
    let suffix = format!("web:rankings:{}", filter.cache_key());
    let fetch = async {
        let (items, cursor) = search_rankings(&engine, event_id, &filter, mode).await?;
        Ok(WebRankingPageSchema {
            items,
            next_cursor: cursor.map(encode_ranking_cursor),
        })
    };
    let response = cached(&state, &server, event_id, suffix, fetch).await?;
    Ok(Json(response))
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id))]
pub async fn world_bloom_rankings(
    State(state): State<AppState>,
    Path((server, event_id, character_id)): Path<(String, i64, i64)>,
    Query(query): Query<RankingSearchQuery>,
) -> Result<Json<WebRankingPageSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_web_user_id_mode(&state, &engine, region, event_id).await?;
    let filter = query.into_filter()?;
    let suffix = format!("web:wb:{character_id}:rankings:{}", filter.cache_key());
    let fetch = async {
        let (items, cursor) =
            search_world_bloom_rankings(&engine, event_id, character_id, &filter, mode).await?;
        Ok(WebRankingPageSchema {
            items,
            next_cursor: cursor.map(encode_ranking_cursor),
        })
    };
    let response = cached(&state, &server, event_id, suffix, fetch).await?;
    Ok(Json(response))
}

#[tracing::instrument(skip(state, query), fields(server, event_id, user_id))]
pub async fn user_trace(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
    Query(query): Query<UserTraceQuery>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_web_user_id_mode(&state, &engine, region, event_id).await?;
    let filter = query.into_filter()?;
    let suffix = format!("web:trace:user:{user_id}:{}", filter.cache_key());
    let fetch = async {
        let rank_data = search_user_trace(&engine, event_id, &user_id, &filter, mode).await?;
        not_found_if_empty(&rank_data)?;
        Ok(UserAllRankingDataQueryResponseSchema {
            rank_data,
            user_data: None,
        })
    };
    let response = cached_trace(&state, &server, event_id, suffix, fetch).await?;
    Ok(Json(response))
}

#[tracing::instrument(skip(state, query), fields(server, event_id, character_id, user_id))]
pub async fn world_bloom_user_trace(
    State(state): State<AppState>,
    Path((server, event_id, character_id, user_id)): Path<(String, i64, i64, String)>,
    Query(query): Query<UserTraceQuery>,
) -> Result<Json<UserAllRankingDataQueryResponseSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_web_user_id_mode(&state, &engine, region, event_id).await?;
    let filter = query.into_filter()?;
    let suffix = format!(
        "web:wb:{character_id}:trace:user:{user_id}:{}",
        filter.cache_key()
    );
    let fetch = async {
        let rank_data =
            search_world_bloom_user_trace(&engine, event_id, character_id, &user_id, &filter, mode)
                .await?;
        not_found_if_empty(&rank_data)?;
        Ok(UserAllRankingDataQueryResponseSchema {
            rank_data,
            user_data: None,
        })
    };
    let response = cached_trace(&state, &server, event_id, suffix, fetch).await?;
    Ok(Json(response))
}

#[tracing::instrument(skip(state, query), fields(server, event_id))]
pub async fn users(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
    Query(query): Query<UserSearchQuery>,
) -> Result<Json<WebUserSearchPageSchema>, ApiError> {
    let (region, engine) = resolve_region_engine(&state, &server)?;
    let mode = prepare_web_user_id_mode(&state, &engine, region, event_id).await?;
    let filter = query.into_filter()?;
    let suffix = format!("web:users:{}", filter.cache_key());
    let fetch = async {
        let (items, cursor) = search_users(&engine, event_id, &filter, mode).await?;
        Ok(WebUserSearchPageSchema {
            items,
            next_cursor: cursor.map(|cursor| cursor.to_string()),
        })
    };
    let response = cached(&state, &server, event_id, suffix, fetch).await?;
    Ok(Json(response))
}

async fn prepare_web_user_id_mode(
    state: &AppState,
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
) -> Result<PublicUserIdMode, ApiError> {
    if !state.anonymizer().is_enabled() {
        return Err(ApiError::BadRequest(
            "web API requires privacy.uid_anonymization.enabled".into(),
        ));
    }
    ensure_user_table_extensions(engine, server, event_id, state.anonymizer()).await?;
    Ok(PublicUserIdMode::Unique)
}

impl RankingSearchQuery {
    fn into_filter(self) -> Result<WebRankingFilter, ApiError> {
        if let (Some(min), Some(max)) = (self.rank_min, self.rank_max)
            && min > max
        {
            return Err(ApiError::BadRequest("rankMin must be <= rankMax".into()));
        }
        if let (Some(min), Some(max)) = (self.score_min, self.score_max)
            && min > max
        {
            return Err(ApiError::BadRequest("scoreMin must be <= scoreMax".into()));
        }
        if let (Some(start), Some(end)) = (self.start_time, self.end_time)
            && start > end
        {
            return Err(ApiError::BadRequest("startTime must be <= endTime".into()));
        }
        Ok(WebRankingFilter {
            rank_min: self.rank_min,
            rank_max: self.rank_max,
            score_min: self.score_min,
            score_max: self.score_max,
            start_time: self.start_time,
            end_time: self.end_time,
            before: self.before,
            after: self.after,
            timestamp: self.timestamp,
            cursor: parse_ranking_cursor(self.cursor.as_deref())?,
            limit: clamp_limit(self.limit, DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT),
        })
    }
}

impl UserTraceQuery {
    fn into_filter(self) -> Result<WebTraceFilter, ApiError> {
        if let (Some(start), Some(end)) = (self.start_time, self.end_time)
            && start > end
        {
            return Err(ApiError::BadRequest("startTime must be <= endTime".into()));
        }
        Ok(WebTraceFilter {
            start_time: self.start_time,
            end_time: self.end_time,
            cursor: self.cursor,
            limit: clamp_limit(self.limit, DEFAULT_TRACE_LIMIT, MAX_TRACE_LIMIT),
        })
    }
}

impl UserSearchQuery {
    fn into_filter(self) -> Result<WebUserSearchFilter, ApiError> {
        if self.unique_id.is_none()
            && self.name.is_none()
            && self.profile_word.is_none()
            && self.card_id.is_none()
            && self.card_level.is_none()
            && self.card_master_rank.is_none()
            && self.card_special_training_status.is_none()
            && self.card_default_image.is_none()
            && self.cheerful_team_id.is_none()
        {
            return Err(ApiError::BadRequest(
                "at least one user search filter is required".into(),
            ));
        }
        validate_search_text(self.name.as_deref(), "name")?;
        validate_search_text(self.profile_word.as_deref(), "profileWord")?;
        Ok(WebUserSearchFilter {
            unique_id: self.unique_id,
            name: self.name,
            profile_word: self.profile_word,
            card_id: self.card_id,
            card_level: self.card_level,
            card_master_rank: self.card_master_rank,
            card_special_training_status: self.card_special_training_status,
            card_default_image: self.card_default_image,
            cheerful_team_id: self.cheerful_team_id,
            cursor: self.cursor,
            limit: clamp_limit(self.limit, DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT),
        })
    }
}

impl WebRankingFilter {
    fn cache_key(&self) -> String {
        format!(
            "rankMin={:?}:rankMax={:?}:scoreMin={:?}:scoreMax={:?}:start={:?}:end={:?}:before={:?}:after={:?}:timestamp={:?}:cursor={:?}:limit={}",
            self.rank_min,
            self.rank_max,
            self.score_min,
            self.score_max,
            self.start_time,
            self.end_time,
            self.before,
            self.after,
            self.timestamp,
            self.cursor,
            self.limit
        )
    }
}

impl WebTraceFilter {
    fn cache_key(&self) -> String {
        format!(
            "start={:?}:end={:?}:cursor={:?}:limit={}",
            self.start_time, self.end_time, self.cursor, self.limit
        )
    }
}

impl WebUserSearchFilter {
    fn cache_key(&self) -> String {
        format!(
            "unique={:?}:name={:?}:word={:?}:card={:?}:level={:?}:mr={:?}:status={:?}:image={:?}:team={:?}:cursor={:?}:limit={}",
            self.unique_id,
            self.name,
            self.profile_word,
            self.card_id,
            self.card_level,
            self.card_master_rank,
            self.card_special_training_status,
            self.card_default_image,
            self.cheerful_team_id,
            self.cursor,
            self.limit
        )
    }
}

async fn cached<T, Fut>(
    state: &AppState,
    server: &str,
    event_id: i64,
    suffix: String,
    fetch: Fut,
) -> Result<T, ApiError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                server,
                event_id,
                suffix,
                cache.ttl(CacheTtl::LatestRank),
                fetch,
            )
            .await
    } else {
        fetch.await
    }
}

async fn cached_trace<T, Fut>(
    state: &AppState,
    server: &str,
    event_id: i64,
    suffix: String,
    fetch: Fut,
) -> Result<T, ApiError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    if let Some(cache) = state.cache() {
        cache
            .get_or_fetch(
                server,
                event_id,
                suffix,
                cache.ttl(CacheTtl::TraceRank),
                fetch,
            )
            .await
    } else {
        fetch.await
    }
}

fn clamp_limit(limit: Option<u64>, default: u64, max: u64) -> u64 {
    limit.unwrap_or(default).clamp(1, max)
}

fn validate_search_text(raw: Option<&str>, field: &str) -> Result<(), ApiError> {
    if let Some(value) = raw
        && value.chars().count() < MIN_SEARCH_LEN
    {
        return Err(ApiError::BadRequest(format!(
            "{field} must be at least {MIN_SEARCH_LEN} characters"
        )));
    }
    Ok(())
}

fn parse_ranking_cursor(raw: Option<&str>) -> Result<Option<WebRankingCursor>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let parts = raw
        .split(':')
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::BadRequest("invalid cursor".into()))?;
    match parts.as_slice() {
        [timestamp, rank, user_id_key] => Ok(Some(WebRankingCursor {
            timestamp: *timestamp,
            rank: *rank,
            user_id_key: *user_id_key,
        })),
        _ => Err(ApiError::BadRequest("invalid cursor".into())),
    }
}

fn encode_ranking_cursor(cursor: WebRankingCursor) -> String {
    format!(
        "{}:{}:{}",
        cursor.timestamp, cursor.rank, cursor.user_id_key
    )
}

fn not_found_if_empty(items: &[RecordedRankData]) -> Result<(), ApiError> {
    if items.is_empty() {
        Err(ApiError::NotFound)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking_cursor_round_trips() {
        let encoded = encode_ranking_cursor(WebRankingCursor {
            timestamp: 10,
            rank: 20,
            user_id_key: 30,
        });
        assert_eq!(encoded, "10:20:30");
        assert_eq!(
            parse_ranking_cursor(Some(&encoded)).unwrap(),
            Some(WebRankingCursor {
                timestamp: 10,
                rank: 20,
                user_id_key: 30,
            })
        );
    }

    #[test]
    fn clamps_limits() {
        assert_eq!(clamp_limit(None, 100, 500), 100);
        assert_eq!(clamp_limit(Some(0), 100, 500), 1);
        assert_eq!(clamp_limit(Some(999), 100, 500), 500);
    }

    #[test]
    fn rejects_tiny_search_text() {
        assert!(validate_search_text(Some("a"), "name").is_err());
        assert!(validate_search_text(Some("ab"), "name").is_ok());
    }
}
