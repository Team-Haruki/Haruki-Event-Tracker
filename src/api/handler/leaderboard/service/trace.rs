use serde::Deserialize;

use crate::api::error::ApiError;
use crate::api::extract::{ApiAudience, prepare_audience_user_id_mode, resolve_region_engine};
use crate::api::handler::web::cached_subject_trace_json;
use crate::api::state::AppState;
use crate::db::engine::DatabaseEngine;
use crate::db::query::ranking::{fetch_latest_ranking, fetch_latest_ranking_by_rank};
use crate::db::query::user::{PublicUserIdMode, get_user_data};
use crate::db::query::web::{
    WebTraceFilter, search_rank_trace, search_user_trace, search_world_bloom_rank_trace,
    search_world_bloom_user_trace,
};
use crate::db::query::world_bloom::fetch_latest_world_bloom_ranking_by_rank;
use crate::model::api::{
    RecordedRankData, SubjectTraceMetaSchema, SubjectTraceResponseSchema, WebRankingItemSchema,
};

use super::util::{meta, rank_of_item, timestamp_of_rank_data, user_id_of_rank_data};

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

#[allow(clippy::too_many_arguments)]
pub(super) async fn build_subject_trace_response(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    subject: String,
    query: SubjectTraceQuery,
    cache_prefix: &str,
    audience: ApiAudience,
) -> Result<SubjectTraceResponseSchema, ApiError> {
    let json = build_subject_trace_json(
        state,
        server,
        event_id,
        character_id,
        subject,
        query,
        cache_prefix,
        audience,
    )
    .await?;
    sonic_rs::from_slice(&json).map_err(|err| {
        tracing::warn!(%err, "api cache decoded invalid subject trace");
        ApiError::ServiceUnavailable("api cache decode failed".into())
    })
}

/// The subject trace as cached JSON bytes (see `cached_subject_trace_json`).
#[allow(clippy::too_many_arguments)]
pub(super) async fn build_subject_trace_json(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    subject: String,
    query: SubjectTraceQuery,
    cache_prefix: &str,
    audience: ApiAudience,
) -> Result<bytes::Bytes, ApiError> {
    let subject_type = query.subject_type.as_deref().unwrap_or("user");
    let include_current = query.include_current.unwrap_or(true);
    // Cloud subjects are raw upstream UIDs; keep those out of the Redis
    // keyspace by hashing. Web subjects are already public unique_ids.
    let subject_key = match audience {
        ApiAudience::Cloud => hashed_subject(&subject),
        ApiAudience::Web => subject.clone(),
    };
    let filter = WebTraceFilter {
        start_time: query.start_time,
        end_time: query.end_time,
        cursor: query.cursor,
        limit: query.limit.map(|limit| limit.clamp(1, MAX_TRACE_LIMIT)),
    };
    let suffix = match character_id {
        Some(character_id) => format!(
            "{cache_prefix}:wb:{character_id}:subject:{subject_type}:{subject_key}:current={include_current}:start={:?}:end={:?}:cursor={:?}:limit={:?}",
            filter.start_time, filter.end_time, filter.cursor, filter.limit
        ),
        None => format!(
            "{cache_prefix}:total:subject:{subject_type}:{subject_key}:current={include_current}:start={:?}:end={:?}:cursor={:?}:limit={:?}",
            filter.start_time, filter.end_time, filter.cursor, filter.limit
        ),
    };
    let cache_server = server.clone();
    let fetch = async {
        let (region, engine) = resolve_region_engine(&state, &server)?;
        let mode =
            prepare_audience_user_id_mode(&state, &engine, region, event_id, audience).await?;
        let ResolvedSubject {
            user_id,
            resolved_rank,
            current,
            kind: subject_kind,
            latest_timestamp,
        } = resolve_subject(
            &engine,
            event_id,
            character_id,
            &subject,
            subject_type,
            mode,
            include_current,
        )
        .await?;
        // A cursor poll past the subject's newest row is the same empty
        // result the range query would produce; answer it without a trace
        // permit or a ranking-table scan.
        if cursor_exhausted(filter.cursor, latest_timestamp) {
            return Err(ApiError::NotFound);
        }
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
                        region,
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
                        region,
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
                    search_rank_trace(&engine, region, event_id, rank, &filter, mode).await?
                }
                SubjectKind::User => {
                    search_user_trace(&engine, region, event_id, &user_id, &filter, mode).await?
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
    cached_subject_trace_json(&state, &cache_server, event_id, suffix, fetch).await
}

pub(super) fn hashed_subject(subject: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(subject.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Whether a cursor poll can only come back empty: the subject's newest
/// row, when the resolution read it, is not past the cursor (the cursor is
/// exclusive). Relies on `time_id` order == `timestamp` order like every
/// reader: the newest row by `time_id` carries the newest timestamp.
fn cursor_exhausted(cursor: Option<i64>, latest_timestamp: Option<i64>) -> bool {
    matches!((cursor, latest_timestamp), (Some(cursor), Some(latest)) if latest <= cursor)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubjectKind {
    User,
    Rank,
}

struct ResolvedSubject {
    user_id: String,
    resolved_rank: Option<i64>,
    current: Option<WebRankingItemSchema>,
    kind: SubjectKind,
    /// The subject's newest row's timestamp, when resolving it read that
    /// row (always for a rank; for a user only with `include_current`).
    latest_timestamp: Option<i64>,
}

async fn resolve_subject(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    subject: &str,
    subject_type: &str,
    mode: PublicUserIdMode,
    include_current: bool,
) -> Result<ResolvedSubject, ApiError> {
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
        let latest_timestamp = Some(timestamp_of_rank_data(&rank_data));
        let current_item = include_current.then_some(WebRankingItemSchema {
            rank_data,
            user_data: None,
        });
        return Ok(ResolvedSubject {
            user_id,
            resolved_rank: Some(rank),
            current: current_item,
            kind: SubjectKind::Rank,
            latest_timestamp,
        });
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
    let latest_timestamp = current
        .as_ref()
        .map(|item| timestamp_of_rank_data(&item.rank_data));
    Ok(ResolvedSubject {
        user_id: subject.to_owned(),
        resolved_rank,
        current,
        kind: SubjectKind::User,
        latest_timestamp,
    })
}

async fn fetch_latest_user_rank(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
    mode: PublicUserIdMode,
) -> Result<Option<WebRankingItemSchema>, ApiError> {
    let latest = fetch_latest_ranking(engine, event_id, user_id, mode).await?;
    Ok(latest.map(|rank| WebRankingItemSchema {
        rank_data: RecordedRankData::Normal(rank),
        user_data: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_exhausted_only_at_or_past_the_newest_row() {
        assert!(cursor_exhausted(Some(100), Some(100)));
        assert!(cursor_exhausted(Some(101), Some(100)));
        assert!(!cursor_exhausted(Some(99), Some(100)));
        assert!(!cursor_exhausted(None, Some(100)));
        assert!(!cursor_exhausted(Some(100), None));
        assert!(!cursor_exhausted(None, None));
    }
}
