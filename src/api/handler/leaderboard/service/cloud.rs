use serde::Deserialize;

use crate::api::error::ApiError;
use crate::api::extract::parse_rank_query;
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::model::api::{
    CloudCheckRoomResponseSchema, CloudLineResponseSchema, CloudRankInfoSchema,
    CloudRankQueryResponseSchema, CloudSpeedResponseSchema, CloudTraceResponseSchema,
    RankSnapshotsResponseSchema, RankingScoreGrowthSchema, RecordedRankData, WebRankingItemSchema,
};

use super::snapshot::{SnapshotBuildRequest, build_rank_snapshots_response};
use super::trace::{SubjectTraceQuery, build_subject_trace_response};
use super::util::{interval_seconds, rank_of_item};
use crate::api::handler::leaderboard::round_metrics;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudQuery {
    user_id: Option<String>,
    interval: Option<i64>,
    unit_seconds: Option<i64>,
    include_adjacent: Option<bool>,
    skip_missing: Option<bool>,
    subject_type: Option<String>,
    subject: Option<String>,
    limit: Option<u64>,
}

pub(crate) async fn cloud_query_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
    raw_query: Option<String>,
    include_round_metrics: bool,
) -> Result<Json<CloudRankQueryResponseSchema>, ApiError> {
    let ranks = parse_rank_query(raw_query.as_deref()).unwrap_or_default();
    let include_adjacent = query.include_adjacent.unwrap_or(true);
    let snapshots = if !ranks.is_empty() {
        build_rank_snapshots_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            SnapshotBuildRequest {
                ranks,
                include_adjacent,
                include_metrics: false,
                interval: interval_seconds(query.interval),
                at: None,
                cache_prefix: "cloud:v2",
            },
        )
        .await?
    } else if let Some(user_id) = query.user_id.as_deref().filter(|id| !id.trim().is_empty()) {
        let subject = build_subject_trace_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            user_id.to_owned(),
            SubjectTraceQuery {
                subject_type: Some("user".to_owned()),
                include_current: Some(true),
                start_time: None,
                end_time: None,
                cursor: None,
                limit: Some(1),
            },
            round_metrics::CLOUD_ROUND_METRICS_CACHE_PREFIX,
        )
        .await?;
        let Some(current) = subject.current else {
            return Err(ApiError::NotFound);
        };
        let rank = rank_of_item(&current).ok_or(ApiError::NotFound)?;
        build_rank_snapshots_response(
            state.clone(),
            server.clone(),
            event_id,
            character_id,
            SnapshotBuildRequest {
                ranks: vec![rank],
                include_adjacent,
                include_metrics: false,
                interval: interval_seconds(query.interval),
                at: None,
                cache_prefix: "cloud:v2",
            },
        )
        .await?
    } else {
        return Err(ApiError::BadRequest("rank or userId is required".into()));
    };
    let skip_missing = query.skip_missing.unwrap_or(false);
    let mut ranks = cloud_infos_from_snapshots(&snapshots, skip_missing)?;
    if include_round_metrics {
        round_metrics::enrich_cloud_rank_infos_with_trace_metrics(
            &state,
            &server,
            event_id,
            character_id,
            &mut ranks,
        )
        .await;
    }
    let first = snapshots.items.first();
    Ok(Json(CloudRankQueryResponseSchema {
        meta: snapshots.meta,
        ranks,
        previous: first
            .and_then(|item| item.previous.as_ref())
            .and_then(cloud_info_from_item),
        next: first
            .and_then(|item| item.next.as_ref())
            .and_then(cloud_info_from_item),
    }))
}

pub(crate) async fn cloud_check_room_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
    raw_query: Option<String>,
) -> Result<Json<CloudCheckRoomResponseSchema>, ApiError> {
    let response = cloud_query_for_scope(
        state,
        server,
        event_id,
        character_id,
        query,
        raw_query,
        true,
    )
    .await?;
    let response = response.0;
    let ranks = response.ranks;
    let rank = ranks.first().cloned().ok_or(ApiError::NotFound)?;
    Ok(Json(CloudCheckRoomResponseSchema {
        meta: response.meta,
        rank,
        ranks,
        previous: response.previous,
        next: response.next,
    }))
}

pub(crate) async fn cloud_line_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    mut query: CloudQuery,
    raw_query: Option<String>,
) -> Result<Json<CloudLineResponseSchema>, ApiError> {
    query.include_adjacent = Some(false);
    let response = cloud_query_for_scope(
        state,
        server,
        event_id,
        character_id,
        query,
        raw_query,
        false,
    )
    .await?;
    let response = response.0;
    Ok(Json(CloudLineResponseSchema {
        meta: response.meta,
        ranks: response
            .ranks
            .into_iter()
            .map(|mut rank| {
                rank.name.clear();
                rank
            })
            .collect(),
    }))
}

pub(crate) async fn cloud_speed_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
    raw_query: Option<String>,
) -> Result<Json<CloudSpeedResponseSchema>, ApiError> {
    let interval = interval_seconds(query.interval);
    let unit_seconds = query.unit_seconds.unwrap_or(3600).clamp(1, 86_400);
    let snapshots = build_rank_snapshots_response(
        state,
        server,
        event_id,
        character_id,
        SnapshotBuildRequest {
            ranks: parse_rank_query(raw_query.as_deref())?,
            include_adjacent: false,
            include_metrics: true,
            interval,
            at: None,
            cache_prefix: "cloud:v2",
        },
    )
    .await?;
    let speeds = cloud_infos_from_snapshots(&snapshots, query.skip_missing.unwrap_or(false))?
        .into_iter()
        .map(|mut info| {
            if let Some(speed) = info.speed {
                info.speed = Some(speed * unit_seconds / interval.max(1));
            }
            info.speed_window = Some(interval);
            info
        })
        .collect();
    Ok(Json(CloudSpeedResponseSchema {
        meta: snapshots.meta,
        speeds,
        interval_seconds: interval,
        unit_seconds,
    }))
}

pub(crate) async fn cloud_trace_for_scope(
    state: AppState,
    server: String,
    event_id: i64,
    character_id: Option<i64>,
    query: CloudQuery,
) -> Result<Json<CloudTraceResponseSchema>, ApiError> {
    let subject_type = query.subject_type.unwrap_or_else(|| "user".to_owned());
    let subject = query
        .subject
        .or(query.user_id)
        .ok_or_else(|| ApiError::BadRequest("subject is required".into()))?;
    let trace = build_subject_trace_response(
        state,
        server,
        event_id,
        character_id,
        subject,
        SubjectTraceQuery {
            subject_type: Some(subject_type),
            include_current: Some(true),
            start_time: None,
            end_time: None,
            cursor: None,
            limit: query.limit,
        },
        round_metrics::CLOUD_ROUND_METRICS_CACHE_PREFIX,
    )
    .await?;
    let name = trace
        .user_data
        .as_ref()
        .map(|user| user.name.clone())
        .or_else(|| {
            trace
                .current
                .as_ref()
                .and_then(|current| current.user_data.as_ref().map(|user| user.name.clone()))
        })
        .unwrap_or_default();
    let rank_data = trace
        .rank_data
        .iter()
        .filter_map(|data| cloud_info_from_rank_data(data, Some(name.as_str()), None))
        .collect();
    Ok(Json(CloudTraceResponseSchema {
        meta: trace.meta,
        subject: trace.subject,
        rank_data,
    }))
}

fn cloud_infos_from_snapshots(
    snapshots: &RankSnapshotsResponseSchema,
    skip_missing: bool,
) -> Result<Vec<CloudRankInfoSchema>, ApiError> {
    let mut out = Vec::with_capacity(snapshots.items.len());
    for item in &snapshots.items {
        if let Some(current) = item.current.as_ref()
            && let Some(info) = cloud_info_from_item_with_metrics(current, item.metrics.as_ref())
        {
            out.push(info);
            continue;
        }
        if !skip_missing {
            return Err(ApiError::NotFound);
        }
    }
    if out.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(out)
}

fn cloud_info_from_item(item: &WebRankingItemSchema) -> Option<CloudRankInfoSchema> {
    cloud_info_from_item_with_metrics(item, None)
}

fn cloud_info_from_item_with_metrics(
    item: &WebRankingItemSchema,
    metrics: Option<&RankingScoreGrowthSchema>,
) -> Option<CloudRankInfoSchema> {
    let name = item
        .user_data
        .as_ref()
        .map(|user| user.name.as_str())
        .unwrap_or_default();
    cloud_info_from_rank_data(&item.rank_data, Some(name), metrics)
}

fn cloud_info_from_rank_data(
    rank_data: &RecordedRankData,
    name: Option<&str>,
    metrics: Option<&RankingScoreGrowthSchema>,
) -> Option<CloudRankInfoSchema> {
    let (rank, user_id, score, timestamp, character_id) = match rank_data {
        RecordedRankData::Normal(data) => (
            data.rank,
            data.user_id.clone(),
            data.score,
            data.timestamp,
            None,
        ),
        RecordedRankData::WorldBloom(data) => (
            data.rank,
            data.user_id.clone(),
            data.score,
            data.timestamp,
            data.character_id,
        ),
    };
    if rank <= 0 {
        return None;
    }
    Some(CloudRankInfoSchema {
        rank,
        user_id: (!user_id.is_empty()).then_some(user_id),
        name: name.unwrap_or_default().to_owned(),
        score,
        timestamp,
        average_round: None,
        average_pt: None,
        latest_pt: metrics.and_then(|metrics| {
            metrics
                .score_earlier
                .map(|earlier| metrics.score_latest - earlier)
        }),
        speed: metrics.and_then(|metrics| metrics.growth),
        min20_times_3_speed: None,
        hour_round: None,
        record_start_at: None,
        speed_window: metrics.and_then(|metrics| metrics.time_diff),
        character_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handler::leaderboard::service::util::meta;
    use sonic_rs::JsonValueTrait;

    #[test]
    fn cloud_rank_info_serializes_round_metric_fields() {
        let info = CloudRankInfoSchema {
            rank: 100,
            user_id: Some("12345".to_owned()),
            name: "User".to_owned(),
            score: 1_500_000,
            timestamp: 1_704_067_200,
            average_round: Some(2),
            average_pt: Some(250_000),
            latest_pt: Some(300_000),
            speed: Some(600_000),
            min20_times_3_speed: Some(900_000),
            hour_round: Some(2),
            record_start_at: Some(1_704_060_000_000),
            speed_window: Some(3_600),
            character_id: Some(25),
        };

        let value = sonic_rs::to_value(&info).expect("serialize rank info");
        assert_eq!(value["averageRound"].as_i64(), Some(2));
        assert_eq!(value["averagePt"].as_i64(), Some(250_000));
        assert_eq!(value["latestPt"].as_i64(), Some(300_000));
        assert_eq!(value["min20Times3Speed"].as_i64(), Some(900_000));
        assert_eq!(value["hourRound"].as_i64(), Some(2));
        assert_eq!(value["recordStartAt"].as_i64(), Some(1_704_060_000_000));
    }

    #[test]
    fn cloud_check_room_response_serializes_batch_ranks() {
        let rank = CloudRankInfoSchema {
            rank: 1,
            user_id: Some("12345".to_owned()),
            name: "User".to_owned(),
            score: 1_550_000,
            timestamp: 1_704_067_200,
            average_round: None,
            average_pt: None,
            latest_pt: None,
            speed: None,
            min20_times_3_speed: None,
            hour_round: None,
            record_start_at: None,
            speed_window: None,
            character_id: None,
        };
        let response = CloudCheckRoomResponseSchema {
            meta: meta("cn", 170, None, 1),
            rank: rank.clone(),
            ranks: vec![rank],
            previous: None,
            next: None,
        };

        let value = sonic_rs::to_value(&response).expect("serialize check-room response");
        assert_eq!(value["rank"]["rank"].as_i64(), Some(1));
        assert_eq!(value["ranks"][0]["rank"].as_i64(), Some(1));
    }
}
