//! `HarukiEventTracker` — per-server orchestrator scheduled by gocron in
//! Go, by `tokio_cron_scheduler` here. Wraps `EventTrackerBase` with the
//! "current event" lifecycle: detect the active event from master data,
//! re-init when the event id rolls forward, handle ended/aggregating
//! short-circuits, and drive the per-chapter World Bloom finalization.
//!
//! Direct port of `tracker/tracker.go`. Owned as
//! `Arc<tokio::sync::Mutex<HarukiEventTracker>>` so the scheduler tick
//! can borrow it mutably for one full pass.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;

use crate::api::realtime::{RealtimeHub, RealtimeTopic};
use crate::db::engine::DatabaseEngine;
use crate::model::enums::{SekaiEventStatus, SekaiEventType, SekaiServerRegion};
use crate::model::event::{EventStatus, WorldBloomChapterStatus};
use crate::privacy::UidAnonymizer;
use crate::sekai_api::client::HarukiSekaiAPIClient;
use crate::tracker::base::{EventTrackerBase, TrackerError, TrackerTuning};
use crate::tracker::parser::{EventDataParser, ParseError};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("master data: {0}")]
    Parse(#[from] ParseError),
    #[error("tracker: {0}")]
    Tracker(#[from] TrackerError),
    #[error("no active event for server {0}")]
    NoActiveEvent(SekaiServerRegion),
}

pub struct HarukiEventTracker {
    server: SekaiServerRegion,
    api: HarukiSekaiAPIClient,
    redis: redis::aio::ConnectionManager,
    api_cache_redis: Option<redis::aio::ConnectionManager>,
    db: Arc<DatabaseEngine>,
    realtime: RealtimeHub,
    anonymizer: UidAnonymizer,
    tuning: TrackerTuning,
    parser: EventDataParser,
    inner: Option<EventTrackerBase>,
}

impl HarukiEventTracker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server: SekaiServerRegion,
        api: HarukiSekaiAPIClient,
        redis: redis::aio::ConnectionManager,
        api_cache_redis: Option<redis::aio::ConnectionManager>,
        db: Arc<DatabaseEngine>,
        realtime: RealtimeHub,
        anonymizer: UidAnonymizer,
        tuning: TrackerTuning,
        master_dir: impl AsRef<str>,
    ) -> Result<Self, ParseError> {
        Ok(Self {
            server,
            parser: EventDataParser::new(server, master_dir)?,
            api,
            redis,
            api_cache_redis,
            db,
            realtime,
            anonymizer,
            tuning,
            inner: None,
        })
    }

    pub fn server(&self) -> SekaiServerRegion {
        self.server
    }

    /// Build a fresh `EventTrackerBase` for the currently-active event and
    /// run its initialization. Mirrors Go `HarukiEventTracker.Init`.
    #[tracing::instrument(skip(self), fields(server = %self.server))]
    pub async fn init(&mut self) -> Result<(), DaemonError> {
        let event = self
            .parser
            .get_current_event_status()
            .await?
            .ok_or(DaemonError::NoActiveEvent(self.server))?;
        let is_event_ended = event.event_status == SekaiEventStatus::Ended;
        let mut base = EventTrackerBase::new(
            self.server,
            event.event_id,
            event.event_type,
            is_event_ended,
            self.db.clone(),
            self.redis.clone(),
            self.api_cache_redis.clone(),
            self.api.clone(),
            self.anonymizer.clone(),
            self.tuning,
            event.chapter_statuses,
        );
        base.init().await?;
        self.inner = Some(base);
        Ok(())
    }

    /// Scheduler entry-point. One tracker tick. Logs errors instead of
    /// surfacing them so a single bad fetch doesn't kill the schedule.
    #[tracing::instrument(skip(self), fields(server = %self.server))]
    pub async fn track_ranking_data(&mut self) {
        let event = match self.parser.get_current_event_status().await {
            Ok(Some(e)) => e,
            Ok(None) => {
                tracing::debug!("no active event, skipping tick");
                return;
            }
            Err(err) => {
                tracing::error!(%err, "failed to read current event status");
                return;
            }
        };

        let need_init = match self.inner.as_ref() {
            None => true,
            Some(base) if base.event_id() < event.event_id => {
                tracing::info!(
                    new_event_id = event.event_id,
                    old_event_id = base.event_id(),
                    "new event detected, switching tracker"
                );
                true
            }
            _ => false,
        };
        if need_init && let Err(err) = self.init().await {
            tracing::error!(%err, "tracker init failed");
            return;
        }

        if self.inner.as_ref().map(|b| b.event_id()) == Some(event.event_id)
            && self.handle_tracker_match(&event).await
        {
            return;
        }

        let Some(base) = self.inner.as_mut() else {
            return;
        };
        tracing::debug!(event_id = event.event_id, "tracking ranking data");
        match base.record_ranking_data(false, false).await {
            Ok(true) => self.notify_update(event.event_id),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(%err, event_id = event.event_id, "record_ranking_data failed")
            }
        }
    }

    /// Returns `true` when the caller should skip the main
    /// `record_ranking_data` for this tick (event already done /
    /// aggregating / just finalized).
    async fn handle_tracker_match(&mut self, event: &EventStatus) -> bool {
        let Some(base) = self.inner.as_mut() else {
            return false;
        };
        if base.is_event_ended() {
            match base.refresh_after_end().await {
                Ok(true) => notify_realtime_update(&self.realtime, self.server, event.event_id),
                Ok(false) => {}
                Err(err) => {
                    tracing::error!(
                        %err,
                        event_id = event.event_id,
                        "post-end refresh failed"
                    );
                }
            }
            return true;
        }
        if event.event_status == SekaiEventStatus::Aggregating {
            tracing::debug!(event_id = event.event_id, "event aggregating, skipping");
            return true;
        }
        let realtime = self.realtime.clone();
        let server = self.server;
        if Self::handle_event_ended(base, event, &realtime, server).await {
            return true;
        }
        if event.event_type == SekaiEventType::WorldBloom {
            Self::handle_world_bloom(base, event, &realtime, server).await;
        }
        false
    }

    async fn handle_event_ended(
        base: &mut EventTrackerBase,
        event: &EventStatus,
        realtime: &RealtimeHub,
        server: SekaiServerRegion,
    ) -> bool {
        if event.event_status != SekaiEventStatus::Ended || base.is_event_ended() {
            return false;
        }
        tracing::info!(event_id = event.event_id, "event ended, finalizing");
        match base.record_ranking_data(false, true).await {
            Ok(true) => notify_realtime_update(realtime, server, event.event_id),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(%err, event_id = event.event_id, "final record_ranking_data failed")
            }
        }
        base.set_event_ended(true).await;
        true
    }

    async fn handle_world_bloom(
        base: &mut EventTrackerBase,
        event: &EventStatus,
        realtime: &RealtimeHub,
        server: SekaiServerRegion,
    ) {
        if !world_bloom_statuses_equal(base.world_bloom_statuses(), &event.chapter_statuses) {
            base.set_world_bloom_statuses(event.chapter_statuses.clone());
        }

        // Iterate every chapter — overlap periods are intentional in Go.
        for (&character_id, detail) in &event.chapter_statuses {
            Self::handle_world_bloom_chapter(base, event, character_id, detail, realtime, server)
                .await;
        }
    }

    async fn handle_world_bloom_chapter(
        base: &mut EventTrackerBase,
        event: &EventStatus,
        character_id: i64,
        detail: &WorldBloomChapterStatus,
        realtime: &RealtimeHub,
        server: SekaiServerRegion,
    ) -> bool {
        match detail.chapter_status {
            SekaiEventStatus::NotStarted => false,
            SekaiEventStatus::Aggregating => {
                tracing::info!(
                    event_id = event.event_id,
                    character_id,
                    "WB chapter aggregating, skipping"
                );
                false
            }
            SekaiEventStatus::Ended => {
                if base.is_world_bloom_chapter_ended(character_id) {
                    return false;
                }
                tracing::info!(
                    event_id = event.event_id,
                    character_id,
                    "WB chapter ended, finalizing"
                );
                match base.record_ranking_data(true, true).await {
                    Ok(true) => notify_realtime_update(realtime, server, event.event_id),
                    Ok(false) => {}
                    Err(err) => {
                        tracing::error!(
                            %err,
                            event_id = event.event_id,
                            character_id,
                            "WB final record_ranking_data failed"
                        );
                    }
                }
                base.set_world_bloom_chapter_ended(character_id, true);
                true
            }
            _ => false,
        }
    }

    fn notify_update(&self, event_id: i64) {
        notify_realtime_update(&self.realtime, self.server, event_id);
    }

    /// Drain the inner tracker's pending write buffer. Called from graceful
    /// shutdown so buffered samples survive a restart.
    pub async fn flush_on_shutdown(&mut self) {
        if let Some(base) = self.inner.as_mut() {
            base.flush_on_shutdown().await;
        }
    }
}

fn notify_realtime_update(realtime: &RealtimeHub, server: SekaiServerRegion, event_id: i64) {
    realtime.notify_update(RealtimeTopic::new(server, event_id), Utc::now().timestamp());
}

fn world_bloom_statuses_equal(
    a: &HashMap<i64, WorldBloomChapterStatus>,
    b: &HashMap<i64, WorldBloomChapterStatus>,
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, v)| {
        b.get(k).is_some_and(|bv| {
            v.server == bv.server
                && v.event_id == bv.event_id
                && v.character_id == bv.character_id
                && v.chapter_status == bv.chapter_status
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::realtime::RealtimeMessage;
    use crate::model::enums::SekaiUnit;
    use crate::model::event::Event;
    use crate::tracker::base::tests::tracker_fixture;
    use sea_orm::{Database, DatabaseBackend};

    fn event_status(
        event_id: i64,
        event_type: SekaiEventType,
        event_status: SekaiEventStatus,
        chapter_statuses: HashMap<i64, WorldBloomChapterStatus>,
    ) -> EventStatus {
        EventStatus {
            server: SekaiServerRegion::Jp,
            event_id,
            event_type,
            event_status,
            remain: String::new(),
            assetbundle_name: "event".into(),
            chapter_statuses,
            detail: Event {
                id: event_id,
                event_type,
                name: "test".into(),
                assetbundle_name: "event".into(),
                bgm_assetbundle_name: String::new(),
                event_only_component_display_start_at: 0,
                start_at: 0,
                aggregate_at: 0,
                ranking_announce_at: 0,
                distribution_start_at: 0,
                event_only_component_display_end_at: 0,
                closed_at: 0,
                distribution_end_at: 0,
                virtual_live_id: 0,
                unit: SekaiUnit::None,
                is_count_leader_character_play: false,
                event_point_assetbundle_name: String::new(),
                standby_screen_display_start_at: 0,
            },
        }
    }

    fn chapter(
        event_id: i64,
        character_id: i64,
        status: SekaiEventStatus,
    ) -> WorldBloomChapterStatus {
        WorldBloomChapterStatus {
            server: SekaiServerRegion::Jp,
            event_id,
            character_id,
            chapter_status: status,
        }
    }

    #[test]
    fn world_bloom_status_comparison_checks_every_field() {
        let original = HashMap::from([(10, chapter(1, 10, SekaiEventStatus::Ongoing))]);
        assert!(world_bloom_statuses_equal(&original, &original));
        assert!(!world_bloom_statuses_equal(&original, &HashMap::new()));
        for changed in [
            chapter(2, 10, SekaiEventStatus::Ongoing),
            chapter(1, 11, SekaiEventStatus::Ongoing),
            chapter(1, 10, SekaiEventStatus::Ended),
            WorldBloomChapterStatus {
                server: SekaiServerRegion::En,
                ..chapter(1, 10, SekaiEventStatus::Ongoing)
            },
        ] {
            assert!(!world_bloom_statuses_equal(
                &original,
                &HashMap::from([(10, changed)])
            ));
        }
    }

    #[tokio::test]
    async fn daemon_handles_empty_master_data_and_aggregating_events() {
        let Ok(redis_url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
            return;
        };
        let client = redis::Client::open(redis_url).unwrap();
        let redis = redis::aio::ConnectionManager::new(client).await.unwrap();
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let db = Arc::new(DatabaseEngine::from_connection(
            conn,
            DatabaseBackend::Sqlite,
        ));
        let root = std::env::temp_dir().join(format!("haruki-daemon-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("events.json"), "[]").unwrap();
        std::fs::write(root.join("worldBlooms.json"), "[]").unwrap();

        let mut daemon = HarukiEventTracker::new(
            SekaiServerRegion::Jp,
            HarukiSekaiAPIClient::new("http://127.0.0.1", "").unwrap(),
            redis,
            None,
            db,
            RealtimeHub::new(),
            UidAnonymizer::disabled(),
            TrackerTuning::default(),
            root.to_string_lossy(),
        )
        .unwrap();
        assert_eq!(daemon.server(), SekaiServerRegion::Jp);
        assert!(matches!(
            daemon.init().await,
            Err(DaemonError::NoActiveEvent(_))
        ));
        daemon.track_ranking_data().await;

        let Some(base) = tracker_fixture(SekaiEventType::Marathon).await else {
            return;
        };
        let event_id = base.event_id();
        daemon.inner = Some(base);
        let aggregating = event_status(
            event_id,
            SekaiEventType::Marathon,
            SekaiEventStatus::Aggregating,
            HashMap::new(),
        );
        assert!(daemon.handle_tracker_match(&aggregating).await);
        let ongoing = event_status(
            event_id,
            SekaiEventType::Marathon,
            SekaiEventStatus::Ongoing,
            HashMap::new(),
        );
        assert!(!daemon.handle_tracker_match(&ongoing).await);
        daemon.inner = None;
        assert!(!daemon.handle_tracker_match(&ongoing).await);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn world_bloom_helpers_skip_inactive_chapters_and_notify() {
        let Some(mut base) = tracker_fixture(SekaiEventType::WorldBloom).await else {
            return;
        };
        let event_id = base.event_id();
        let realtime = RealtimeHub::new();
        let mut receiver = realtime.subscribe();
        notify_realtime_update(&realtime, SekaiServerRegion::Jp, event_id);
        let RealtimeMessage::Updated { topic, .. } = receiver.recv().await.unwrap() else {
            panic!("expected update");
        };
        assert_eq!(topic, RealtimeTopic::new(SekaiServerRegion::Jp, event_id));

        let statuses = HashMap::from([
            (10, chapter(event_id, 10, SekaiEventStatus::NotStarted)),
            (11, chapter(event_id, 11, SekaiEventStatus::Aggregating)),
        ]);
        let event = event_status(
            event_id,
            SekaiEventType::WorldBloom,
            SekaiEventStatus::Ongoing,
            statuses,
        );
        HarukiEventTracker::handle_world_bloom(&mut base, &event, &realtime, SekaiServerRegion::Jp)
            .await;
        assert!(
            !HarukiEventTracker::handle_event_ended(
                &mut base,
                &event,
                &realtime,
                SekaiServerRegion::Jp,
            )
            .await
        );
        assert!(
            !HarukiEventTracker::handle_world_bloom_chapter(
                &mut base,
                &event,
                10,
                &chapter(event_id, 10, SekaiEventStatus::NotStarted),
                &realtime,
                SekaiServerRegion::Jp,
            )
            .await
        );
    }
}
