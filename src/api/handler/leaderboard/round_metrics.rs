use chrono::{DateTime, Utc};

use crate::api::extract::{prepare_user_id_mode, resolve_region_engine};
use crate::api::state::AppState;
use crate::db::query::web::{WebTraceFilter, search_user_trace, search_world_bloom_user_trace};
use crate::model::api::{CloudRankInfoSchema, RecordedRankData};

const CLOUD_TRACE_METRICS_LOOKBACK_SECONDS: i64 = 12 * 60 * 60;
const CLOUD_RECOVERY_IDLE_SECONDS: i64 = 5 * 60;
const TRACKER_REALTIME_TAIL_MAX_LAG_SECONDS: i64 = 30 * 24 * 60 * 60;

pub(super) const CLOUD_ROUND_METRICS_CACHE_PREFIX: &str =
    "cloud:v2:roundMetrics=v7-fullTraceRecoveryRt";

pub(super) async fn enrich_cloud_rank_infos_with_trace_metrics(
    state: &AppState,
    server: &str,
    event_id: i64,
    character_id: Option<i64>,
    ranks: &mut [CloudRankInfoSchema],
) {
    let Ok((region, engine)) = resolve_region_engine(state, server) else {
        return;
    };
    let Ok(mode) = prepare_user_id_mode(state, &engine, region, event_id).await else {
        return;
    };
    for rank in ranks {
        if has_cloud_round_metrics(rank) {
            continue;
        }
        let Some(user_id) = rank
            .user_id
            .as_deref()
            .filter(|user_id| !user_id.is_empty())
        else {
            continue;
        };
        let user_id = user_id.to_owned();
        let Ok(_permit) = state.query_limiter().acquire_trace(region).await else {
            continue;
        };
        let filter = cloud_trace_metrics_filter(rank.timestamp);
        let trace = match character_id {
            Some(character_id) => {
                search_world_bloom_user_trace(
                    &engine,
                    event_id,
                    character_id,
                    user_id.as_str(),
                    &filter,
                    mode,
                )
                .await
            }
            None => search_user_trace(&engine, event_id, user_id.as_str(), &filter, mode).await,
        };
        let Ok(trace) = trace else {
            continue;
        };
        apply_cloud_trace_metrics_at(rank, &trace, Utc::now());
    }
}

fn cloud_trace_metrics_filter(rank_timestamp: i64) -> WebTraceFilter {
    let end_time = positive_timestamp(Some(normalize_tracker_unix_seconds(rank_timestamp)));
    WebTraceFilter {
        start_time: None,
        end_time,
        cursor: None,
        limit: None,
    }
}

fn has_cloud_round_metrics(info: &CloudRankInfoSchema) -> bool {
    info.average_round.is_some()
        && info.average_pt.is_some()
        && info.latest_pt.is_some()
        && info.hour_round.is_some()
        && info.min20_times_3_speed.is_some()
        && info.speed.is_some()
        && info.record_start_at.is_some()
}

#[derive(Debug, Clone, Copy)]
struct CloudTraceSample {
    score: i64,
    timestamp: i64,
}

fn apply_cloud_trace_metrics_at(
    info: &mut CloudRankInfoSchema,
    trace: &[RecordedRankData],
    now: DateTime<Utc>,
) {
    let mut samples = trace
        .iter()
        .filter_map(cloud_trace_sample)
        .filter(|sample| sample.timestamp > 0)
        .collect::<Vec<_>>();
    if samples.is_empty() {
        return;
    }
    samples.sort_by_key(|sample| normalize_tracker_unix_seconds(sample.timestamp));

    if samples.len() < 2 {
        return;
    }
    if let Some(record_start_at) = recovery_record_start_at(&samples) {
        info.record_start_at = Some(format_tracker_timestamp(record_start_at));
    }

    let deltas = samples
        .windows(2)
        .filter_map(|window| {
            let diff = window[1].score - window[0].score;
            (diff > 0).then_some(diff)
        })
        .collect::<Vec<_>>();
    if let Some(latest) = deltas.last().copied() {
        info.latest_pt = Some(latest);
        let avg_window = if deltas.len() > 10 {
            &deltas[deltas.len() - 10..]
        } else {
            &deltas[..]
        };
        let round_count = avg_window.len() as i64;
        if round_count > 0 {
            let sum = avg_window.iter().sum::<i64>();
            info.average_round = Some(round_count);
            info.average_pt = Some(sum / round_count);
        }
    }

    let Some(last) = samples.last().copied() else {
        return;
    };
    let end_sec = effective_tracker_window_end_unix_seconds(last.timestamp, now);
    let metrics_start = end_sec - CLOUD_TRACE_METRICS_LOOKBACK_SECONDS;
    let metrics_start_idx = find_window_baseline_index(&samples, metrics_start).unwrap_or(0);
    let metric_samples = &samples[metrics_start_idx..];
    if metric_samples.len() < 2 {
        return;
    }

    let hour_start = end_sec - 60 * 60;
    if let Some(hour_base_idx) = find_window_baseline_index(metric_samples, hour_start) {
        let hour_base = metric_samples[hour_base_idx];
        let hour_base_sec = normalize_tracker_unix_seconds(hour_base.timestamp);
        if end_sec > hour_base_sec {
            let hour_gain = (last.score - hour_base.score).max(0);
            let hour_elapsed = end_sec - hour_base_sec;
            info.speed = Some(hour_gain * 3600 / hour_elapsed);
        }
        info.hour_round = Some(count_positive_deltas(&metric_samples[hour_base_idx..]));
    }

    let window_start = end_sec - 20 * 60;
    if let Some(window_base_idx) = find_window_baseline_index(metric_samples, window_start) {
        let window_base = metric_samples[window_base_idx];
        let window_gain = (last.score - window_base.score).max(0);
        info.min20_times_3_speed = Some(window_gain * 3);
    }
}

fn cloud_trace_sample(rank_data: &RecordedRankData) -> Option<CloudTraceSample> {
    match rank_data {
        RecordedRankData::Normal(data) => Some(CloudTraceSample {
            score: data.score,
            timestamp: data.timestamp,
        }),
        RecordedRankData::WorldBloom(data) => Some(CloudTraceSample {
            score: data.score,
            timestamp: data.timestamp,
        }),
    }
}

fn normalize_tracker_unix_seconds(timestamp: i64) -> i64 {
    if timestamp > 1_000_000_000_000 {
        timestamp / 1000
    } else {
        timestamp
    }
}

fn format_tracker_timestamp(timestamp: i64) -> i64 {
    if timestamp > 1_000_000_000_000 {
        timestamp
    } else {
        timestamp * 1000
    }
}

fn effective_tracker_window_end_unix_seconds(last_timestamp: i64, now: DateTime<Utc>) -> i64 {
    let last_sec = normalize_tracker_unix_seconds(last_timestamp);
    if last_sec <= 0 {
        return last_sec;
    }
    let now_sec = now.timestamp();
    if now_sec <= last_sec || now_sec - last_sec > TRACKER_REALTIME_TAIL_MAX_LAG_SECONDS {
        return last_sec;
    }
    now_sec
}

fn find_window_baseline_index(samples: &[CloudTraceSample], window_start: i64) -> Option<usize> {
    if samples.is_empty() {
        return None;
    }
    let mut baseline = None;
    for (idx, sample) in samples.iter().enumerate() {
        let sec = normalize_tracker_unix_seconds(sample.timestamp);
        if sec <= window_start {
            baseline = Some(idx);
            continue;
        }
        break;
    }
    Some(baseline.unwrap_or(0))
}

fn count_positive_deltas(samples: &[CloudTraceSample]) -> i64 {
    samples
        .windows(2)
        .filter(|window| window[1].score - window[0].score > 0)
        .count() as i64
}

fn recovery_record_start_at(samples: &[CloudTraceSample]) -> Option<i64> {
    if samples.is_empty() {
        return None;
    }
    let mut latest_recovery = samples[0].timestamp;
    let mut flat_start = samples[0];
    let mut in_flat = false;
    for window in samples.windows(2) {
        let previous = window[0];
        let current = window[1];
        if current.score == previous.score {
            in_flat = true;
        } else if current.score > previous.score {
            let idle_seconds = normalize_tracker_unix_seconds(current.timestamp)
                - normalize_tracker_unix_seconds(flat_start.timestamp);
            if in_flat && idle_seconds >= CLOUD_RECOVERY_IDLE_SECONDS {
                latest_recovery = current.timestamp;
            }
            flat_start = current;
            in_flat = false;
        } else if current.score < previous.score {
            flat_start = current;
            in_flat = false;
        }
    }
    Some(latest_recovery)
}

fn positive_timestamp(timestamp: Option<i64>) -> Option<i64> {
    timestamp.filter(|timestamp| *timestamp > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn cloud_trace_metrics_match_cloud_fallback_semantics() {
        let trace = vec![
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_000_000,
                timestamp: 1_704_060_000,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_250_000,
                timestamp: 1_704_063_600,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_550_000,
                timestamp: 1_704_067_200,
            }),
        ];
        let mut info = cloud_info_fixture(100, 1_550_000, 1_704_067_200);

        apply_cloud_trace_metrics_at(
            &mut info,
            &trace,
            Utc.timestamp_opt(1_704_067_200, 0).single().unwrap(),
        );

        assert_eq!(info.record_start_at, Some(1_704_060_000_000));
        assert_eq!(info.latest_pt, Some(300_000));
        assert_eq!(info.average_round, Some(2));
        assert_eq!(info.average_pt, Some(275_000));
        assert_eq!(info.speed, Some(300_000));
        assert_eq!(info.hour_round, Some(1));
        assert_eq!(info.min20_times_3_speed, Some(900_000));
    }

    #[test]
    fn cloud_trace_metrics_use_recovery_start_time() {
        let trace = vec![
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_250_000,
                timestamp: 1_704_063_600,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 100,
                user_id: "12345".to_owned(),
                score: 1_550_000,
                timestamp: 1_704_067_200,
            }),
        ];
        let mut info = cloud_info_fixture(100, 1_550_000, 1_704_067_200);
        info.record_start_at = Some(1_704_000_000_000);

        apply_cloud_trace_metrics_at(
            &mut info,
            &trace,
            Utc.timestamp_opt(1_704_067_200, 0).single().unwrap(),
        );

        assert_eq!(info.record_start_at, Some(1_704_063_600_000));
        assert_eq!(info.latest_pt, Some(300_000));
        assert_eq!(info.speed, Some(300_000));
    }

    #[test]
    fn cloud_trace_metrics_keep_rt_after_recent_recovery() {
        let trace = vec![
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 1_000_000,
                timestamp: 1_704_060_000,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 1_000_000,
                timestamp: 1_704_060_360,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 1_300_000,
                timestamp: 1_704_060_420,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 1_600_000,
                timestamp: 1_704_067_200,
            }),
        ];
        let mut info = cloud_info_fixture(1, 1_600_000, 1_704_067_200);

        apply_cloud_trace_metrics_at(
            &mut info,
            &trace,
            Utc.timestamp_opt(1_704_067_200, 0).single().unwrap(),
        );

        assert_eq!(info.record_start_at, Some(1_704_060_420_000));
    }

    #[test]
    fn cloud_trace_metrics_keep_rt_before_recent_metric_window() {
        let trace = vec![
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 1_000_000,
                timestamp: 1_704_000_000,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 1_000_000,
                timestamp: 1_704_000_360,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 1_300_000,
                timestamp: 1_704_000_420,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 2_000_000,
                timestamp: 1_704_063_600,
            }),
            RecordedRankData::Normal(crate::model::api::RecordedRankingSchema {
                rank: 1,
                user_id: "12345".to_owned(),
                score: 2_300_000,
                timestamp: 1_704_067_200,
            }),
        ];
        let mut info = cloud_info_fixture(1, 2_300_000, 1_704_067_200);

        apply_cloud_trace_metrics_at(
            &mut info,
            &trace,
            Utc.timestamp_opt(1_704_067_200, 0).single().unwrap(),
        );

        assert_eq!(info.record_start_at, Some(1_704_000_420_000));
        assert_eq!(info.speed, Some(300_000));
        assert_eq!(info.hour_round, Some(1));
    }

    #[test]
    fn cloud_trace_metrics_filter_looks_back_from_current_rank_time() {
        let filter = cloud_trace_metrics_filter(1_704_067_200);
        assert_eq!(filter.end_time, Some(1_704_067_200));
        assert_eq!(filter.start_time, None);
        assert_eq!(filter.cursor, None);
        assert_eq!(filter.limit, None);
    }

    #[test]
    fn tracker_window_end_ignores_stale_tail() {
        let last_timestamp = 1_704_067_200;
        let now = Utc
            .timestamp_opt(
                last_timestamp + TRACKER_REALTIME_TAIL_MAX_LAG_SECONDS + 1,
                0,
            )
            .single()
            .unwrap();

        assert_eq!(
            effective_tracker_window_end_unix_seconds(last_timestamp, now),
            last_timestamp
        );
    }

    fn cloud_info_fixture(rank: i64, score: i64, timestamp: i64) -> CloudRankInfoSchema {
        CloudRankInfoSchema {
            rank,
            user_id: Some("12345".to_owned()),
            name: "User".to_owned(),
            score,
            timestamp,
            average_round: None,
            average_pt: None,
            latest_pt: None,
            speed: None,
            min20_times_3_speed: None,
            hour_round: None,
            record_start_at: None,
            speed_window: None,
            character_id: None,
        }
    }
}
