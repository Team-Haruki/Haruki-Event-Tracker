use serde::Deserialize;

use crate::api::error::ApiError;
use crate::api::extract::{prepare_user_id_mode, resolve_region_engine};
use crate::api::handler::web::cached_trace;
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::db::query::ranking::{fetch_all_rankings, fetch_latest_ranking_by_rank};
use crate::db::query::user::{PublicUserIdMode, get_user_data};
use crate::db::query::web::{
    WebTraceFilter, search_rank_trace, search_user_trace, search_world_bloom_rank_trace,
    search_world_bloom_user_trace,
};
use crate::db::query::world_bloom::fetch_latest_world_bloom_ranking_by_rank;
use crate::model::api::{
    RecordedRankData, SubjectTraceMetaSchema, SubjectTraceResponseSchema, WebRankingItemSchema,
};

use super::util::{meta, rank_of_item, user_id_of_rank_data};

const MAX_TRACE_LIMIT: u64 = 10_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubjectTraceQuery {
    pub(super) subject_type: Option<String>,
    pub(super) include_current: Option<bool>,
    pub(super) start_time: Option<i64>,
    pub(super) end_time: Option<i64>,
    pub(super) cursor: Option<i64>,
    pub(super) limit: Option<u64>,
}

pub(super) async fn build_subject_trace_response(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    subject: String,
    query: SubjectTraceQuery,
    cache_prefix: &str,
) -> Result<SubjectTraceResponseSchema, ApiError> {
    let subject_type = query.subject_type.as_deref().unwrap_or("user");
    let include_current = query.include_current.unwrap_or(true);
    let filter = WebTraceFilter {
        start_time: query.start_time,
        end_time: query.end_time,
        cursor: query.cursor,
        limit: query.limit.map(|limit| limit.clamp(1, MAX_TRACE_LIMIT)),
    };
    let suffix = match character_id {
        Some(character_id) => format!(
            "{cache_prefix}:wb:{character_id}:subject:{subject_type}:{subject}:current={include_current}:start={:?}:end={:?}:cursor={:?}:limit={:?}",
            filter.start_time, filter.end_time, filter.cursor, filter.limit
        ),
        None => format!(
            "{cache_prefix}:total:subject:{subject_type}:{subject}:current={include_current}:start={:?}:end={:?}:cursor={:?}:limit={:?}",
            filter.start_time, filter.end_time, filter.cursor, filter.limit
        ),
    };
    let cache_server = server.clone();
    let fetch = async {
        let (region, engine) = resolve_region_engine(&state, &server)?;
        let mode = prepare_user_id_mode(&state, &engine, region, event_id).await?;
        let (user_id, resolved_rank, current, subject_kind) = resolve_subject(
            &engine,
            event_id,
            character_id,
            &subject,
            subject_type,
            mode,
            include_current,
        )
        .await?;
        let limiter = state.query_limiter().clone();
        let _permit = limiter.acquire_trace(region).await?;
        let rank_data = match character_id {
            Some(character_id) => match subject_kind {
                SubjectKind::Rank => {
                    let rank = resolved_rank.ok_or_else(|| {
                        ApiError::ServiceUnavailable("rank subject has no resolved rank".into())
                    })?;
                    search_world_bloom_rank_trace(
                        &engine,
                        event_id,
                        character_id,
                        rank,
                        &filter,
                        mode,
                    )
                    .await?
                }
                SubjectKind::User => {
                    search_world_bloom_user_trace(
                        &engine,
                        event_id,
                        character_id,
                        &user_id,
                        &filter,
                        mode,
                    )
                    .await?
                }
            },
            None => match subject_kind {
                SubjectKind::Rank => {
                    let rank = resolved_rank.ok_or_else(|| {
                        ApiError::ServiceUnavailable("rank subject has no resolved rank".into())
                    })?;
                    search_rank_trace(&engine, event_id, rank, &filter, mode).await?
                }
                SubjectKind::User => {
                    search_user_trace(&engine, event_id, &user_id, &filter, mode).await?
                }
            },
        };
        if rank_data.is_empty() {
            return Err(ApiError::NotFound);
        }
        let user_data = get_user_data(&engine, event_id, &user_id, mode)
            .await
            .ok()
            .flatten();
        Ok(SubjectTraceResponseSchema {
            meta: meta(
                &server,
                event_id,
                character_id,
                chrono::Utc::now().timestamp(),
            ),
            subject: SubjectTraceMetaSchema {
                subject_type: subject_type.to_owned(),
                subject,
                resolved_user_id: Some(user_id),
                resolved_rank,
            },
            current,
            rank_data,
            user_data,
        })
    };
    cached_trace(&state, &cache_server, event_id, suffix, fetch).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubjectKind {
    User,
    Rank,
}

async fn resolve_subject(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    subject: &str,
    subject_type: &str,
    mode: PublicUserIdMode,
    include_current: bool,
) -> Result<
    (
        String,
        Option<i64>,
        Option<WebRankingItemSchema>,
        SubjectKind,
    ),
    ApiError,
> {
    if subject_type.eq_ignore_ascii_case("rank") {
        let rank = subject
            .parse::<i64>()
            .map_err(|_| ApiError::BadRequest("rank subject must be an integer".into()))?;
        if rank <= 0 {
            return Err(ApiError::BadRequest("rank subject must be positive".into()));
        }
        let current = match character_id {
            Some(character_id) => {
                fetch_latest_world_bloom_ranking_by_rank(engine, event_id, rank, character_id, mode)
                    .await?
                    .map(RecordedRankData::WorldBloom)
            }
            None => fetch_latest_ranking_by_rank(engine, event_id, rank, mode)
                .await?
                .map(RecordedRankData::Normal),
        };
        let Some(rank_data) = current else {
            return Err(ApiError::NotFound);
        };
        let user_id = user_id_of_rank_data(&rank_data).ok_or_else(|| {
            ApiError::ServiceUnavailable("latest rank response has no user id".into())
        })?;
        let current_item = include_current.then_some(WebRankingItemSchema {
            rank_data,
            user_data: None,
        });
        return Ok((user_id, Some(rank), current_item, SubjectKind::Rank));
    }
    if !subject_type.eq_ignore_ascii_case("user") {
        return Err(ApiError::BadRequest(
            "subjectType must be user or rank".into(),
        ));
    }
    let current = if include_current {
        match character_id {
            Some(character_id) => {
                let latest = crate::db::query::world_bloom::fetch_latest_world_bloom_ranking(
                    engine,
                    event_id,
                    subject,
                    character_id,
                    mode,
                )
                .await?
                .map(RecordedRankData::WorldBloom);
                latest.map(|rank_data| WebRankingItemSchema {
                    rank_data,
                    user_data: None,
                })
            }
            None => fetch_latest_user_rank(engine, event_id, subject, mode).await?,
        }
    } else {
        None
    };
    let resolved_rank = current.as_ref().and_then(rank_of_item);
    Ok((
        subject.to_owned(),
        resolved_rank,
        current,
        SubjectKind::User,
    ))
}

async fn fetch_latest_user_rank(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
    mode: PublicUserIdMode,
) -> Result<Option<WebRankingItemSchema>, ApiError> {
    let rows = fetch_all_rankings(engine, event_id, user_id, mode).await?;
    Ok(rows.into_iter().last().map(|rank| WebRankingItemSchema {
        rank_data: RecordedRankData::Normal(rank),
        user_data: None,
    }))
}
