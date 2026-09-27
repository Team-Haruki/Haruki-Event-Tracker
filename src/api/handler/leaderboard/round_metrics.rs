use chrono::{DateTime, Utc};
use futures::StreamExt;
use futures::stream;

use crate::api::extract::{ApiAudience, prepare_audience_user_id_mode, resolve_region_engine};
use crate::api::state::AppState;
use crate::db::query::score_samples::{ScoreSample, fetch_user_score_samples};
use crate::model::api::CloudRankInfoSchema;

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
    let Ok(mode) =
        prepare_audience_user_id_mode(state, &engine, region, event_id, ApiAudience::Cloud).await
    else {
        return;
    };
    let jobs: Vec<(usize, String, i64)> = ranks
        .iter()
        .enumerate()
        .filter(|(_, rank)| !has_cloud_round_metrics(rank))
        .filter_map(|(idx, rank)| {
            rank.user_id
                .as_deref()
                .filter(|user_id| !user_id.is_empty())
                .map(|user_id| (idx, user_id.to_owned(), rank.timestamp))
        })
        .collect();
    if jobs.is_empty() {
        return;
    }
    // Per-rank fetches are independent; run them concurrently but bounded, so
    // one 100-rank batch can't park 100 waiters on the trace-permit queue
    // ahead of every other request.
    // The whole history up to the rank's own sample: `record_start_at`
    // looks for the last idle gap anywhere in it, so no lookback window.
    let samples: Vec<_> = stream::iter(jobs.into_iter().map(|(idx, user_id, timestamp)| {
        let engine = engine.clone();
        async move {
            let Ok(_permit) = state.query_limiter().acquire_trace(region).await else {
                return (idx, None);
            };
            let samples = fetch_user_score_samples(
                &engine,
                event_id,
                character_id,
                user_id.as_str(),
                cloud_trace_metrics_end(timestamp),
                mode,
            )
            .await;
            (idx, samples.ok())
        }
    }))
    .buffer_unordered(state.query_limiter().batch_trace_fill_concurrency())
    .collect()
    .await;
    let now = Utc::now();
    for (idx, samples) in samples {
        if let Some(samples) = samples {
            apply_cloud_trace_metrics_at(&mut ranks[idx], &samples, now);
        }
    }
}

fn cloud_trace_metrics_end(rank_timestamp: i64) -> Option<i64> {
    positive_timestamp(Some(normalize_tracker_unix_seconds(rank_timestamp)))
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

fn apply_cloud_trace_metrics_at(
    info: &mut CloudRankInfoSchema,
    trace: &[ScoreSample],
    now: DateTime<Utc>,
) {
    let mut samples = trace
        .iter()
        .copied()
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

fn find_window_baseline_index(samples: &[ScoreSample], window_start: i64) -> Option<usize> {
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

fn count_positive_deltas(samples: &[ScoreSample]) -> i64 {
    samples
        .windows(2)
        .filter(|window| window[1].score - window[0].score > 0)
        .count() as i64
}

fn recovery_record_start_at(samples: &[ScoreSample]) -> Option<i64> {
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
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::realtime::RealtimeHub;
    use crate::api::ws_ticket::WsTicketStore;
    use crate::config::ApiQueryConfig;
    use crate::db::query::user::PublicUserIdMode;
    use crate::db::query::web::tests::{
        seed_normal_event_with_history, seed_player_pushed_out,
        seed_world_bloom_event_with_history, sqlite_engine,
    };
    use crate::db::query::web::{WebTraceFilter, search_user_trace, search_world_bloom_user_trace};
    use crate::db::schema::create_event_tables;
    use crate::model::api::RecordedRankData;
    use crate::model::enums::SekaiServerRegion;
    use crate::privacy::UidAnonymizer;
    use chrono::TimeZone;
    use std::collections::HashMap;
    use std::sync::Arc;

    const NORMAL_EVENT: i64 = 821;
    const WORLD_BLOOM_EVENT: i64 = 822;

    async fn test_state() -> AppState {
        let engine = sqlite_engine().await;
        create_event_tables(&engine, SekaiServerRegion::Jp, NORMAL_EVENT, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, NORMAL_EVENT).await;
        seed_player_pushed_out(&engine, NORMAL_EVENT, None, 3).await;
        create_event_tables(&engine, SekaiServerRegion::Jp, WORLD_BLOOM_EVENT, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_history(&engine, WORLD_BLOOM_EVENT).await;
        seed_player_pushed_out(&engine, WORLD_BLOOM_EVENT, Some(17), 3).await;
        AppState::new(
            HashMap::from([(SekaiServerRegion::Jp, Arc::new(engine))]),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), [SekaiServerRegion::Jp]),
            UidAnonymizer::disabled(),
            None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
    }

    /// The lean samples query feeds the metrics exactly what the full user
    /// trace did. The seeds have sequence `time_id`s (1, 2, …) that differ
    /// from their timestamps, so the timestamp must come from the time
    /// table; player 300 was pushed out (history but no current rank),
    /// 999 was never seen.
    #[tokio::test]
    async fn lean_samples_yield_the_metrics_of_the_full_trace() {
        let state = test_state().await;
        let (_, engine) = resolve_region_engine(&state, "jp").unwrap();
        let end = 1_710_000_120;
        for (event, chapter) in [(NORMAL_EVENT, None), (WORLD_BLOOM_EVENT, Some(17))] {
            for user in ["100", "300", "400", "999"] {
                let filter = WebTraceFilter {
                    start_time: None,
                    end_time: Some(end),
                    cursor: None,
                    limit: None,
                };
                let trace = match chapter {
                    Some(chapter) => search_world_bloom_user_trace(
                        &engine,
                        SekaiServerRegion::Jp,
                        event,
                        chapter,
                        user,
                        &filter,
                        PublicUserIdMode::Raw,
                    )
                    .await
                    .unwrap(),
                    None => search_user_trace(
                        &engine,
                        SekaiServerRegion::Jp,
                        event,
                        user,
                        &filter,
                        PublicUserIdMode::Raw,
                    )
                    .await
                    .unwrap(),
                };
                let trace_samples: Vec<ScoreSample> = trace
                    .iter()
                    .map(|row| match row {
                        RecordedRankData::Normal(row) => ScoreSample {
                            score: row.score,
                            timestamp: row.timestamp,
                        },
                        RecordedRankData::WorldBloom(row) => ScoreSample {
                            score: row.score,
                            timestamp: row.timestamp,
                        },
                    })
                    .collect();
                let samples = fetch_user_score_samples(
                    &engine,
                    event,
                    chapter,
                    user,
                    cloud_trace_metrics_end(end),
                    PublicUserIdMode::Raw,
                )
                .await
                .unwrap();
                assert_eq!(samples, trace_samples, "{event}/{chapter:?} user {user}");
                assert_eq!(samples.is_empty(), user == "999");

                let now = Utc::now();
                let mut expected = cloud_info_fixture(3, 0, end);
                expected.user_id = Some(user.to_owned());
                apply_cloud_trace_metrics_at(&mut expected, &trace_samples, now);
                let mut ranks = vec![cloud_info_fixture(3, 0, end)];
                ranks[0].user_id = Some(user.to_owned());
                enrich_cloud_rank_infos_with_trace_metrics(
                    &state, "jp", event, chapter, &mut ranks,
                )
                .await;
                assert_eq!(
                    sonic_rs::to_string(&ranks[0]).unwrap(),
                    sonic_rs::to_string(&expected).unwrap(),
                    "{event}/{chapter:?} user {user}"
                );
                // Not vacuous: two rows 60 s apart yield a round of +300.
                if user == "100" {
                    assert_eq!(ranks[0].latest_pt, Some(300));
                    assert_eq!(ranks[0].record_start_at, Some(1_710_000_000_000));
                }
            }
        }
    }

    #[test]
    fn cloud_trace_metrics_match_cloud_fallback_semantics() {
        let trace = vec![
            ScoreSample {
                score: 1_000_000,
                timestamp: 1_704_060_000,
            },
            ScoreSample {
                score: 1_250_000,
                timestamp: 1_704_063_600,
            },
            ScoreSample {
                score: 1_550_000,
                timestamp: 1_704_067_200,
            },
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
            ScoreSample {
                score: 1_250_000,
                timestamp: 1_704_063_600,
            },
            ScoreSample {
                score: 1_550_000,
                timestamp: 1_704_067_200,
            },
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
            ScoreSample {
                score: 1_000_000,
                timestamp: 1_704_060_000,
            },
            ScoreSample {
                score: 1_000_000,
                timestamp: 1_704_060_360,
            },
            ScoreSample {
                score: 1_300_000,
                timestamp: 1_704_060_420,
            },
            ScoreSample {
                score: 1_600_000,
                timestamp: 1_704_067_200,
            },
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
            ScoreSample {
                score: 1_000_000,
                timestamp: 1_704_000_000,
            },
            ScoreSample {
                score: 1_000_000,
                timestamp: 1_704_000_360,
            },
            ScoreSample {
                score: 1_300_000,
                timestamp: 1_704_000_420,
            },
            ScoreSample {
                score: 2_000_000,
                timestamp: 1_704_063_600,
            },
            ScoreSample {
                score: 2_300_000,
                timestamp: 1_704_067_200,
            },
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
    fn cloud_trace_metrics_end_at_the_current_rank_time() {
        assert_eq!(cloud_trace_metrics_end(1_704_067_200), Some(1_704_067_200));
        assert_eq!(
            cloud_trace_metrics_end(1_704_067_200_000),
            Some(1_704_067_200)
        );
        assert_eq!(cloud_trace_metrics_end(0), None);
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
