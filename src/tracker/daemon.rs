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
use std::path::PathBuf;
use std::sync::Arc;

use crate::db::engine::DatabaseEngine;
use crate::model::enums::{SekaiEventStatus, SekaiEventType, SekaiServerRegion};
use crate::model::event::{EventStatus, WorldBloomChapterStatus};
use crate::sekai_api::client::HarukiSekaiAPIClient;
use crate::tracker::base::{EventTrackerBase, TrackerError};
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
    db: Arc<DatabaseEngine>,
    parser: EventDataParser,
    inner: Option<EventTrackerBase>,
}

impl HarukiEventTracker {
    pub fn new(
        server: SekaiServerRegion,
        api: HarukiSekaiAPIClient,
        redis: redis::aio::ConnectionManager,
        db: Arc<DatabaseEngine>,
        master_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            server,
            parser: EventDataParser::new(server, master_dir),
            api,
            redis,
            db,
            inner: None,
        }
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
            self.api.clone(),
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
                tracing::info!("no active event, skipping tick");
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
        if need_init
            && let Err(err) = self.init().await
        {
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
        tracing::info!(event_id = event.event_id, "tracking ranking data");
        if let Err(err) = base.record_ranking_data(false).await {
            tracing::error!(%err, event_id = event.event_id, "record_ranking_data failed");
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
            tracing::info!(event_id = event.event_id, "event already ended, skipping");
            return true;
        }
        if event.event_status == SekaiEventStatus::Aggregating {
            tracing::info!(event_id = event.event_id, "event aggregating, skipping");
            return true;
        }
        if Self::handle_event_ended(base, event).await {
            return true;
        }
        if event.event_type == SekaiEventType::WorldBloom {
            Self::handle_world_bloom(base, event).await;
        }
        false
    }

    async fn handle_event_ended(base: &mut EventTrackerBase, event: &EventStatus) -> bool {
        if event.event_status != SekaiEventStatus::Ended || base.is_event_ended() {
            return false;
        }
        tracing::info!(event_id = event.event_id, "event ended, finalizing");
        if let Err(err) = base.record_ranking_data(false).await {
            tracing::error!(%err, event_id = event.event_id, "final record_ranking_data failed");
        }
        base.set_event_ended(true).await;
        true
    }

    async fn handle_world_bloom(base: &mut EventTrackerBase, event: &EventStatus) {
        if !world_bloom_statuses_equal(base.world_bloom_statuses(), &event.chapter_statuses) {
            base.set_world_bloom_statuses(event.chapter_statuses.clone());
        }

        // Iterate every chapter — overlap periods are intentional in Go.
        for (&character_id, detail) in &event.chapter_statuses {
            Self::handle_world_bloom_chapter(base, event, character_id, detail).await;
        }
    }

    async fn handle_world_bloom_chapter(
        base: &mut EventTrackerBase,
        event: &EventStatus,
        character_id: i64,
        detail: &WorldBloomChapterStatus,
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
                if let Err(err) = base.record_ranking_data(true).await {
                    tracing::error!(
                        %err,
                        event_id = event.event_id,
                        character_id,
                        "WB final record_ranking_data failed"
                    );
                }
                base.set_world_bloom_chapter_ended(character_id, true);
                true
            }
            _ => false,
        }
    }
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
