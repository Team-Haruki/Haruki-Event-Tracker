//! `EventTrackerBase` — per-(server, event) state machine. Direct port
//! of `tracker/trackerbase.go`'s `EventTrackerBase`.
//!
//! Owned by `tracker::daemon::HarukiEventTracker` which holds it inside a
//! `tokio::sync::Mutex` so the cron-scheduler tick can borrow it
//! mutably for the duration of one tick.
//!
//! The Go version carried `prevEventState` and `prevUserState` maps and a
//! `lastUpdateTime` field that turned out to be dead code (never read by
//! any diff path; `getFilterFunc` always returned `_ => true`). Those are
//! intentionally not ported.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use thiserror::Error;

use crate::db::engine::DatabaseEngine;
use crate::db::query::batch::{batch_insert_event_rankings, batch_insert_world_bloom_rankings};
use crate::db::query::heartbeat::write_heartbeat;
use crate::db::schema::create_event_tables;
use crate::model::enums::{SekaiEventType, SekaiServerRegion};
use crate::model::event::WorldBloomChapterStatus;
use crate::model::sekai::{BorderRankingResponse, PlayerRankingSchema, Top100RankingResponse};
use crate::model::tracker::{
    HandledRankingData, PlayerState, RankState, WorldBloomKey,
};
use crate::sekai_api::client::HarukiSekaiAPIClient;
use crate::sekai_api::error::SekaiApiError;
use crate::tracker::cache::detect_cache;
use crate::tracker::diff::{
    build_event_records, build_world_bloom_rows, diff_rank_based, extract_world_bloom_rankings,
    merge_rankings,
};
use crate::tracker::state::{
    check_event_ended_flag, load_rank_state, save_rank_state, set_event_ended_flag,
};

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("database: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("sekai api: {0}")]
    Api(#[from] SekaiApiError),
}

pub struct EventTrackerBase {
    server: SekaiServerRegion,
    event_id: i64,
    event_type: SekaiEventType,
    is_event_ended: bool,
    world_bloom_statuses: HashMap<i64, WorldBloomChapterStatus>,
    is_world_bloom_chapter_ended: HashMap<i64, bool>,
    db: Arc<DatabaseEngine>,
    redis: redis::aio::ConnectionManager,
    api: HarukiSekaiAPIClient,
    prev_rank_state: HashMap<i64, RankState>,
    prev_world_bloom_state: HashMap<WorldBloomKey, PlayerState>,
}

impl EventTrackerBase {
    pub fn new(
        server: SekaiServerRegion,
        event_id: i64,
        event_type: SekaiEventType,
        is_event_ended: bool,
        db: Arc<DatabaseEngine>,
        redis: redis::aio::ConnectionManager,
        api: HarukiSekaiAPIClient,
        world_bloom_statuses: HashMap<i64, WorldBloomChapterStatus>,
    ) -> Self {
        let is_world_bloom_chapter_ended =
            if event_type == SekaiEventType::WorldBloom && !world_bloom_statuses.is_empty() {
                world_bloom_statuses.keys().map(|&c| (c, false)).collect()
            } else {
                HashMap::new()
            };
        Self {
            server,
            event_id,
            event_type,
            is_event_ended,
            world_bloom_statuses,
            is_world_bloom_chapter_ended,
            db,
            redis,
            api,
            prev_rank_state: HashMap::new(),
            prev_world_bloom_state: HashMap::new(),
        }
    }

    pub fn server(&self) -> SekaiServerRegion {
        self.server
    }
    pub fn event_id(&self) -> i64 {
        self.event_id
    }
    pub fn event_type(&self) -> SekaiEventType {
        self.event_type
    }
    pub fn is_event_ended(&self) -> bool {
        self.is_event_ended
    }
    pub fn world_bloom_statuses(&self) -> &HashMap<i64, WorldBloomChapterStatus> {
        &self.world_bloom_statuses
    }
    pub fn set_world_bloom_statuses(
        &mut self,
        statuses: HashMap<i64, WorldBloomChapterStatus>,
    ) {
        // Preserve "already finalized" entries; new chapters default to false.
        let mut next: HashMap<i64, bool> = statuses
            .keys()
            .map(|&c| (c, self.is_world_bloom_chapter_ended.get(&c).copied().unwrap_or(false)))
            .collect();
        std::mem::swap(&mut self.is_world_bloom_chapter_ended, &mut next);
        self.world_bloom_statuses = statuses;
    }
    pub fn is_world_bloom_chapter_ended(&self, character_id: i64) -> bool {
        self.is_world_bloom_chapter_ended
            .get(&character_id)
            .copied()
            .unwrap_or(false)
    }
    pub fn set_world_bloom_chapter_ended(&mut self, character_id: i64, ended: bool) {
        self.is_world_bloom_chapter_ended
            .insert(character_id, ended);
    }

    /// Phase-1 init for a new tracker instance. Runs the ended-flag
    /// recovery, loads `rank_state` from Redis, and ensures the per-event
    /// SQL tables exist. Mirrors Go `EventTrackerBase.Init`.
    #[tracing::instrument(skip(self), fields(server = %self.server, event_id = self.event_id))]
    pub async fn init(&mut self) -> Result<(), TrackerError> {
        tracing::info!("initializing tracker");

        if check_event_ended_flag(&mut self.redis, self.server, self.event_id).await? {
            tracing::info!("event ended flag found in Redis, skipping initialization");
            self.is_event_ended = true;
            return Ok(());
        }

        match load_rank_state(&mut self.redis, self.server, self.event_id).await {
            Ok(state) => {
                tracing::info!(n = state.len(), "loaded rank state from Redis");
                self.prev_rank_state = state;
            }
            Err(err) => {
                tracing::warn!(%err, "failed to load rank_state from Redis");
            }
        }

        create_event_tables(
            &self.db,
            self.server,
            self.event_id,
            self.event_type == SekaiEventType::WorldBloom,
        )
        .await?;
        tracing::info!("tracker initialized");
        Ok(())
    }

    /// Mark the event as ended and write the Redis flag. Used by the
    /// daemon when `EventDataParser` reports the event window has closed.
    #[tracing::instrument(skip(self), fields(server = %self.server, event_id = self.event_id, ended))]
    pub async fn set_event_ended(&mut self, ended: bool) {
        self.is_event_ended = ended;
        if ended {
            if let Err(err) =
                set_event_ended_flag(&mut self.redis, self.server, self.event_id).await
            {
                tracing::warn!(%err, "failed to write ended flag");
            }
        }
    }

    /// One tracker tick. Fetches upstream, diffs, persists, writes
    /// heartbeat-on-no-change. `only_world_bloom = true` skips the main
    /// top-100 path so the daemon can finalize a single ended chapter
    /// without touching the main event table.
    #[tracing::instrument(skip(self), fields(server = %self.server, event_id = self.event_id, only_world_bloom))]
    pub async fn record_ranking_data(
        &mut self,
        only_world_bloom: bool,
    ) -> Result<(), TrackerError> {
        if self.is_event_ended {
            tracing::info!("event already ended, skipping");
            return Ok(());
        }

        let now = Utc::now().timestamp();

        let data = match self.handle_ranking_data().await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(%err, "API error, writing heartbeat status=1");
                write_heartbeat(&self.db, self.event_id, now, 1).await?;
                return Err(err);
            }
        };

        tracing::info!("recording ranking data");
        let mut batch_called = false;
        let mut changed_ranks: HashMap<i64, RankState> = HashMap::new();

        if !only_world_bloom && !data.rankings.is_empty() {
            let (idx, changed) =
                diff_rank_based(&data.rankings, &mut self.prev_rank_state);
            changed_ranks = changed;
            let diffed: Vec<&PlayerRankingSchema> =
                idx.iter().map(|&i| &data.rankings[i]).collect();
            let records = build_event_records(data.record_time, &diffed);
            if !records.is_empty() {
                batch_insert_event_rankings(&self.db, self.event_id, &records).await?;
                batch_called = true;
            }
        }

        let wl_rows = build_world_bloom_rows(data.record_time, &data.world_bloom_rankings);
        if !wl_rows.is_empty() {
            batch_insert_world_bloom_rankings(
                &self.db,
                self.event_id,
                &wl_rows,
                &mut self.prev_world_bloom_state,
            )
            .await?;
            batch_called = true;
        }

        if !batch_called {
            write_heartbeat(&self.db, self.event_id, now, 0).await?;
        }

        if let Err(err) =
            save_rank_state(&mut self.redis, self.server, self.event_id, &changed_ranks).await
        {
            tracing::warn!(%err, "failed to save rank_state to Redis");
        }
        tracing::info!("finished recording ranking data");
        Ok(())
    }

    async fn handle_ranking_data(&mut self) -> Result<HandledRankingData, TrackerError> {
        let top100: Top100RankingResponse =
            self.api.get_top100(self.server, self.event_id).await?;
        let (border_hash, border): ([u8; 32], BorderRankingResponse) =
            self.api.get_border(self.server, self.event_id).await?;

        let record_time = Utc::now().timestamp();
        let main_top100 = top100.rankings;
        let main_border = border.border_rankings;

        let world_bloom_rankings = if self.event_type == SekaiEventType::WorldBloom {
            extract_world_bloom_rankings(
                top100.user_world_bloom_chapter_rankings,
                border.user_world_bloom_chapter_ranking_borders,
                &self.world_bloom_statuses,
                &self.is_world_bloom_chapter_ended,
            )
        } else {
            HashMap::new()
        };

        let cache_key = format!(
            "{}-event-{}-main-border",
            self.server, self.event_id
        );
        let is_cached = match detect_cache(&mut self.redis, &cache_key, &border_hash).await {
            Ok(hit) => hit,
            Err(err) => {
                tracing::warn!(%err, "border cache check failed; treating as miss");
                false
            }
        };

        let rankings = if is_cached {
            main_top100
        } else {
            merge_rankings(main_top100, main_border)
        };

        Ok(HandledRankingData {
            record_time,
            rankings,
            world_bloom_rankings,
        })
    }
}
