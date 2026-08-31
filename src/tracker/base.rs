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

use crate::api::cache::{abort_event_update, begin_event_update, finish_event_update};
use crate::db::engine::DatabaseEngine;
use crate::db::privacy::ensure_user_table_extensions;
use crate::db::query::batch::{
    batch_insert_event_rankings, batch_insert_world_bloom_rankings, batch_upsert_event_users,
};
use crate::db::query::heartbeat::write_heartbeat;
use crate::db::schema::create_event_tables;
use crate::model::enums::{SekaiEventType, SekaiServerRegion};
use crate::model::event::WorldBloomChapterStatus;
use crate::model::sekai::{BorderRankingResponse, PlayerRankingSchema, Top100RankingResponse};
use crate::model::tracker::{
    HandledRankingData, PlayerEventRankingRecordSchema, PlayerState,
    PlayerWorldBloomRankingRecordSchema, RankState, WorldBloomKey,
};
use crate::privacy::UidAnonymizer;
use crate::sekai_api::client::HarukiSekaiAPIClient;
use crate::sekai_api::error::SekaiApiError;
use crate::tracker::cache::{check_cache, store_cache};
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

/// Per-server tick tuning from the `tracker` config section. All intervals
/// are seconds; `0` means "every tick" (the pre-tuning behaviour).
#[derive(Debug, Clone, Copy)]
pub struct TrackerTuning {
    pub post_end_user_refresh_interval_secs: u64,
    /// Minimum spacing between status-only heartbeat rows (idle / API-error
    /// ticks). A status *transition* always writes immediately. Data-writing
    /// ticks create their own `time_id` row and count as a heartbeat.
    pub idle_heartbeat_interval_secs: u64,
    /// Minimum spacing between upstream border fetches. Between fetches a
    /// tick tracks top-100 only, halving upstream request volume at
    /// second-level cadence; border ranks simply keep their last state.
    pub border_fetch_interval_secs: u64,
    /// Sampling/persistence decoupling window: diffed rows accumulate in
    /// memory (keeping their per-sample timestamps, so trace resolution is
    /// unaffected) and land in one batch per window. `0` writes every tick.
    pub flush_interval_secs: u64,
    /// Flush early once this many rows are pending (memory bound).
    pub flush_max_rows: usize,
    /// Flush immediately when a change touches rank <= this value, keeping
    /// the top of the leaderboard second-fresh during sprints. `0` disables.
    pub flush_hot_ranks: u64,
}

impl Default for TrackerTuning {
    fn default() -> Self {
        Self {
            post_end_user_refresh_interval_secs: 3600,
            idle_heartbeat_interval_secs: 30,
            border_fetch_interval_secs: 0,
            flush_interval_secs: 0,
            flush_max_rows: 2000,
            flush_hot_ranks: 0,
        }
    }
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
    api_cache_redis: Option<redis::aio::ConnectionManager>,
    api: HarukiSekaiAPIClient,
    anonymizer: UidAnonymizer,
    tuning: TrackerTuning,
    last_post_end_user_refresh_at: Option<i64>,
    /// `(written_at, status)` of the last heartbeat-equivalent row, used to
    /// throttle status-only heartbeats at second-level cadence.
    last_heartbeat: Option<(i64, i16)>,
    last_border_fetch_at: Option<i64>,
    /// Local mirror of the Redis border-hash cache. This tracker is the
    /// only writer, so a match here skips the per-tick Redis GET; `None`
    /// (fresh process) falls back to Redis to resume across restarts.
    last_border_hash: Option<[u8; 32]>,
    /// Diffed-but-not-yet-flushed rows, each keeping its own sample
    /// timestamp. `prev_rank_state` / `wl_sample_state` advance at *sample*
    /// time so the next tick diffs against what is already pending; Redis
    /// `rank_state` and the border hash advance only on flush, so a crash
    /// loses at most one window of intermediate points and converges on
    /// restart exactly like a failed write does today.
    pending_records: Vec<PlayerEventRankingRecordSchema>,
    pending_wl_rows: Vec<PlayerWorldBloomRankingRecordSchema>,
    pending_changed_ranks: HashMap<i64, RankState>,
    pending_border_cache: Option<(String, [u8; 32])>,
    pending_since: Option<i64>,
    pending_hot: bool,
    prev_rank_state: HashMap<i64, RankState>,
    /// Sample-time World Bloom baseline keyed by `(character_id, uid)`.
    /// The flushed baseline (`prev_world_bloom_state`) is keyed by DB
    /// `user_id_key` and only advances inside the batch insert, so it can't
    /// dedupe rows that are still pending in memory — this map can.
    wl_sample_state: HashMap<(i64, i64), PlayerState>,
    prev_world_bloom_state: HashMap<WorldBloomKey, PlayerState>,
    /// `uid -> user_id_key` learned from earlier ticks. Lets the World Bloom
    /// pre-diff drop unchanged rows before their profiles are deep-cloned
    /// and serialized; misses just mean the row is treated as changed.
    wl_user_keys: HashMap<i64, i64>,
}

impl EventTrackerBase {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server: SekaiServerRegion,
        event_id: i64,
        event_type: SekaiEventType,
        is_event_ended: bool,
        db: Arc<DatabaseEngine>,
        redis: redis::aio::ConnectionManager,
        api_cache_redis: Option<redis::aio::ConnectionManager>,
        api: HarukiSekaiAPIClient,
        anonymizer: UidAnonymizer,
        tuning: TrackerTuning,
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
            api_cache_redis,
            api,
            anonymizer,
            tuning,
            last_post_end_user_refresh_at: None,
            last_heartbeat: None,
            last_border_fetch_at: None,
            last_border_hash: None,
            pending_records: Vec::new(),
            pending_wl_rows: Vec::new(),
            pending_changed_ranks: HashMap::new(),
            pending_border_cache: None,
            pending_since: None,
            pending_hot: false,
            wl_sample_state: HashMap::new(),
            prev_rank_state: HashMap::new(),
            prev_world_bloom_state: HashMap::new(),
            wl_user_keys: HashMap::new(),
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
    pub fn set_world_bloom_statuses(&mut self, statuses: HashMap<i64, WorldBloomChapterStatus>) {
        // Preserve "already finalized" entries; new chapters default to false.
        let mut next: HashMap<i64, bool> = statuses
            .keys()
            .map(|&c| {
                (
                    c,
                    self.is_world_bloom_chapter_ended
                        .get(&c)
                        .copied()
                        .unwrap_or(false),
                )
            })
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
        ensure_user_table_extensions(&self.db, self.server, self.event_id, &self.anonymizer)
            .await?;
        tracing::info!("tracker initialized");
        Ok(())
    }

    /// Mark the event as ended and write the Redis flag. Used by the
    /// daemon when `EventDataParser` reports the event window has closed.
    #[tracing::instrument(skip(self), fields(server = %self.server, event_id = self.event_id, ended))]
    pub async fn set_event_ended(&mut self, ended: bool) {
        self.is_event_ended = ended;
        if ended
            && let Err(err) =
                set_event_ended_flag(&mut self.redis, self.server, self.event_id).await
        {
            tracing::warn!(%err, "failed to write ended flag");
        }
    }

    /// Low-frequency post-end refresh. One upstream fetch feeds two steps:
    /// the regular diff-based persist (so late corrections — banned-account
    /// cleanups, final border settlement — land as new trace points for both
    /// the top-100 and border ranks) and the user-dimension upsert (names /
    /// profiles). Returns whether any ranking rows changed.
    #[tracing::instrument(skip(self), fields(server = %self.server, event_id = self.event_id))]
    pub async fn refresh_after_end(&mut self) -> Result<bool, TrackerError> {
        let now = Utc::now().timestamp();
        if !self.should_refresh_user_profiles_after_end(now) {
            tracing::debug!("post-end refresh interval not reached");
            return Ok(false);
        }

        tracing::info!("running post-end low-frequency refresh");
        let data = self.handle_ranking_data().await?;
        // Post-end runs hourly: no reason to coalesce, flush immediately.
        let changed = self
            .persist_ranking_data(&data, false, true, false, now)
            .await?;

        let records = collect_visible_user_records(data.record_time, &data);
        if records.is_empty() {
            self.last_post_end_user_refresh_at = Some(now);
            return Ok(changed);
        }

        if let Some(conn) = self.api_cache_redis.as_mut()
            && let Err(err) = begin_event_update(conn, self.server, self.event_id).await
        {
            tracing::warn!(%err, "failed to mark API cache dirty");
        }

        if let Err(err) = batch_upsert_event_users(
            &self.db,
            self.server,
            self.event_id,
            &self.anonymizer,
            &records,
        )
        .await
        {
            if let Some(conn) = self.api_cache_redis.as_mut()
                && let Err(redis_err) = abort_event_update(conn, self.server, self.event_id).await
            {
                tracing::warn!(%redis_err, "failed to clear API cache dirty after user refresh error");
            }
            return Err(err.into());
        }

        if let Some(conn) = self.api_cache_redis.as_mut()
            && let Err(err) = finish_event_update(conn, self.server, self.event_id).await
        {
            tracing::warn!(%err, "failed to bump API cache epoch after user refresh");
        }
        self.last_post_end_user_refresh_at = Some(now);
        Ok(changed)
    }

    fn should_refresh_user_profiles_after_end(&self, now: i64) -> bool {
        should_refresh_after_end(
            self.last_post_end_user_refresh_at,
            now,
            self.tuning.post_end_user_refresh_interval_secs,
        )
    }

    /// Throttled status-only heartbeat (idle / API-error ticks). A status
    /// transition always writes so the `/status` endpoint sees failures and
    /// recoveries immediately; steady-state repeats are spaced by
    /// `idle_heartbeat_interval_secs` to keep the `time_id` table from
    /// growing one row per second while nothing happens.
    async fn write_status_heartbeat(&mut self, now: i64, status: i16) -> Result<(), TrackerError> {
        if !should_write_status_heartbeat(
            self.last_heartbeat,
            now,
            status,
            self.tuning.idle_heartbeat_interval_secs,
        ) {
            return Ok(());
        }
        write_heartbeat(&self.db, self.event_id, now, status).await?;
        self.last_heartbeat = Some((now, status));
        Ok(())
    }

    /// One tracker tick. Fetches upstream, diffs into the pending buffer,
    /// and flushes when the flush policy (or `force_flush`) says so.
    /// `only_world_bloom = true` skips the main top-100 path so the daemon
    /// can finalize a single ended chapter without touching the main event
    /// table; finalize paths pass `force_flush = true` so terminal rows
    /// never wait out a window.
    #[tracing::instrument(skip(self), fields(server = %self.server, event_id = self.event_id, only_world_bloom, force_flush))]
    pub async fn record_ranking_data(
        &mut self,
        only_world_bloom: bool,
        force_flush: bool,
    ) -> Result<bool, TrackerError> {
        if self.is_event_ended {
            tracing::info!("event already ended, skipping");
            return Ok(false);
        }

        let now = Utc::now().timestamp();

        let data = match self.handle_ranking_data().await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(%err, "API error, writing heartbeat status=1");
                self.write_status_heartbeat(now, 1).await?;
                return Err(err);
            }
        };

        self.persist_ranking_data(&data, only_world_bloom, force_flush, true, now)
            .await
    }

    /// Diff an already-fetched payload into the pending buffer, then flush
    /// when due. Returns whether a flush wrote rows (the daemon's realtime
    /// notify fires on that — clients re-query the DB, which only changes
    /// on flush). `write_idle_heartbeat` controls the freshness row on
    /// quiet ticks: live tracking wants it, the post-end refresh must not
    /// fake liveness with it.
    async fn persist_ranking_data(
        &mut self,
        data: &HandledRankingData,
        only_world_bloom: bool,
        force_flush: bool,
        write_idle_heartbeat: bool,
        now: i64,
    ) -> Result<bool, TrackerError> {
        tracing::debug!("recording ranking data");
        self.accumulate_sample(data, only_world_bloom);

        if !self.has_pending_rows() {
            if write_idle_heartbeat {
                self.write_status_heartbeat(now, 0).await?;
            }
            return Ok(false);
        }
        if !self.should_flush(now, force_flush) {
            return Ok(false);
        }
        let flushed = self.flush_pending(write_idle_heartbeat, now).await?;
        tracing::debug!("finished recording ranking data");
        Ok(flushed)
    }

    /// Diff this sample against the sample-time baselines and append the
    /// changed rows to the pending buffer. Baselines advance here — the
    /// buffer holds the rows until flush, so a failed flush retries them
    /// without re-diffing (the ranking inserts' DO NOTHING dedups any
    /// partially-landed rows).
    fn accumulate_sample(&mut self, data: &HandledRankingData, only_world_bloom: bool) {
        let (changed_ranks, records) = self.build_main_records(data, only_world_bloom);
        if !changed_ranks.is_empty() {
            if self.tuning.flush_hot_ranks > 0 {
                let hot = self.tuning.flush_hot_ranks as i64;
                if changed_ranks.keys().any(|&rank| rank <= hot) {
                    self.pending_hot = true;
                }
            }
            self.prev_rank_state.extend(
                changed_ranks
                    .iter()
                    .map(|(rank, state)| (*rank, state.clone())),
            );
            self.pending_changed_ranks.extend(changed_ranks);
            self.pending_records.extend(records);
        }

        let wl_rows = self.build_world_bloom_records(data);
        for row in &wl_rows {
            if let Ok(uid) = row.base.user_id.parse::<i64>() {
                self.wl_sample_state.insert(
                    (row.character_id, uid),
                    PlayerState {
                        score: row.base.score,
                        rank: row.base.rank,
                    },
                );
            }
        }
        self.pending_wl_rows.extend(wl_rows);

        if !only_world_bloom && let Some((cache_key, border_hash)) = &data.border_cache {
            // Sample-time advance dedups the merge on following ticks; the
            // Redis copy (restart resume) is only written on flush.
            self.last_border_hash = Some(*border_hash);
            self.pending_border_cache = Some((cache_key.clone(), *border_hash));
        }

        if self.has_pending_rows() && self.pending_since.is_none() {
            self.pending_since = Some(data.record_time);
        }
    }

    fn has_pending_rows(&self) -> bool {
        !self.pending_records.is_empty() || !self.pending_wl_rows.is_empty()
    }

    fn should_flush(&self, now: i64, force: bool) -> bool {
        if force || self.tuning.flush_interval_secs == 0 || self.pending_hot {
            return true;
        }
        if self.pending_records.len() + self.pending_wl_rows.len() >= self.tuning.flush_max_rows {
            return true;
        }
        should_refresh_after_end(self.pending_since, now, self.tuning.flush_interval_secs)
    }

    /// Write the pending buffer in one batch. On success the flushed-state
    /// side effects run (Redis `rank_state`, border hash, epoch bump); on
    /// failure the rows are put back and retried on the next flush trigger.
    async fn flush_pending(
        &mut self,
        write_idle_heartbeat: bool,
        now: i64,
    ) -> Result<bool, TrackerError> {
        let records = std::mem::take(&mut self.pending_records);
        let wl_rows = std::mem::take(&mut self.pending_wl_rows);
        self.begin_cache_update(!records.is_empty() || !wl_rows.is_empty())
            .await;
        let batch_called = match self.persist_main_records(&records).await {
            Ok(called) => called,
            Err(err) => {
                self.pending_records = records;
                self.pending_wl_rows = wl_rows;
                return Err(err);
            }
        };
        let batch_called = match self
            .persist_world_bloom_records(&wl_rows, batch_called)
            .await
        {
            Ok(called) => called,
            Err(err) => {
                // The main rows may already have landed; re-inserting them on
                // retry is a DO NOTHING no-op, so putting both back is safe.
                self.pending_records = records;
                self.pending_wl_rows = wl_rows;
                return Err(err);
            }
        };
        self.complete_cache_update(batch_called, write_idle_heartbeat, now)
            .await?;

        let changed_ranks = std::mem::take(&mut self.pending_changed_ranks);
        if !changed_ranks.is_empty()
            && let Err(err) =
                save_rank_state(&mut self.redis, self.server, self.event_id, &changed_ranks).await
        {
            tracing::warn!(%err, "failed to save rank_state to Redis");
        }
        if let Some((cache_key, border_hash)) = self.pending_border_cache.take()
            && let Err(err) = store_cache(&mut self.redis, &cache_key, &border_hash).await
        {
            tracing::warn!(%err, "failed to store border cache hash");
        }
        self.pending_since = None;
        self.pending_hot = false;
        Ok(batch_called)
    }

    /// Flush whatever is pending regardless of the window. Called on
    /// graceful shutdown so a kill during a long flush window doesn't drop
    /// the buffered rows; errors are logged, not surfaced — shutdown
    /// proceeds either way.
    pub async fn flush_on_shutdown(&mut self) {
        if !self.has_pending_rows() {
            return;
        }
        tracing::info!(
            pending_main = self.pending_records.len(),
            pending_world_bloom = self.pending_wl_rows.len(),
            "flushing pending rows before shutdown"
        );
        if let Err(err) = self.flush_pending(false, Utc::now().timestamp()).await {
            tracing::error!(%err, "shutdown flush failed; buffered rows lost");
        }
    }

    fn build_main_records(
        &self,
        data: &HandledRankingData,
        only_world_bloom: bool,
    ) -> (HashMap<i64, RankState>, Vec<PlayerEventRankingRecordSchema>) {
        if only_world_bloom || data.rankings.is_empty() {
            return (HashMap::new(), Vec::new());
        }
        let (indices, changed) = diff_rank_based(&data.rankings, &self.prev_rank_state);
        let diffed: Vec<&PlayerRankingSchema> =
            indices.iter().map(|&index| &data.rankings[index]).collect();
        (changed, build_event_records(data.record_time, &diffed))
    }

    fn build_world_bloom_records(
        &self,
        data: &HandledRankingData,
    ) -> Vec<PlayerWorldBloomRankingRecordSchema> {
        build_world_bloom_rows(
            data.record_time,
            &data.world_bloom_rankings,
            |character_id, uid, score, rank| {
                // Sample-time baseline first: it also covers rows still
                // waiting in the pending buffer.
                if self
                    .wl_sample_state
                    .get(&(character_id, uid))
                    .is_some_and(|state| state.score == score && state.rank == rank)
                {
                    return true;
                }
                let Some(&user_id_key) = self.wl_user_keys.get(&uid) else {
                    return false;
                };
                self.prev_world_bloom_state
                    .get(&WorldBloomKey {
                        user_id_key,
                        character_id,
                    })
                    .is_some_and(|state| state.score == score && state.rank == rank)
            },
        )
    }

    async fn begin_cache_update(&mut self, will_write: bool) {
        if will_write
            && let Some(conn) = self.api_cache_redis.as_mut()
            && let Err(err) = begin_event_update(conn, self.server, self.event_id).await
        {
            tracing::warn!(%err, "failed to mark API cache dirty");
        }
    }

    async fn persist_main_records(
        &mut self,
        records: &[PlayerEventRankingRecordSchema],
    ) -> Result<bool, TrackerError> {
        if !records.is_empty()
            && let Err(err) = batch_insert_event_rankings(
                &self.db,
                self.server,
                self.event_id,
                &self.anonymizer,
                records,
            )
            .await
        {
            self.abort_cache_update("failed to clear API cache dirty after insert error")
                .await;
            return Err(err.into());
        }
        Ok(!records.is_empty())
    }

    async fn persist_world_bloom_records(
        &mut self,
        records: &[PlayerWorldBloomRankingRecordSchema],
        batch_called: bool,
    ) -> Result<bool, TrackerError> {
        if records.is_empty() {
            return Ok(batch_called);
        }
        let result = batch_insert_world_bloom_rankings(
            &self.db,
            self.server,
            self.event_id,
            &self.anonymizer,
            records,
            &mut self.prev_world_bloom_state,
            &mut self.wl_user_keys,
        )
        .await;
        match result {
            Ok(inserted) if inserted > 0 => Ok(true),
            Ok(_) => {
                if !batch_called {
                    self.abort_cache_update(
                        "failed to clear API cache dirty after no-op world bloom insert",
                    )
                    .await;
                }
                Ok(batch_called)
            }
            Err(err) => {
                if batch_called {
                    self.finish_cache_update("failed to bump API cache epoch after partial write")
                        .await;
                } else {
                    self.abort_cache_update("failed to clear API cache dirty after insert error")
                        .await;
                }
                Err(err.into())
            }
        }
    }

    async fn complete_cache_update(
        &mut self,
        batch_called: bool,
        write_idle_heartbeat: bool,
        now: i64,
    ) -> Result<(), TrackerError> {
        if batch_called {
            self.finish_cache_update("failed to bump API cache epoch")
                .await;
            // The batch itself created a status-0 `time_id` row, so this
            // tick counts as a heartbeat for throttling purposes.
            self.last_heartbeat = Some((now, 0));
        } else if write_idle_heartbeat {
            self.write_status_heartbeat(now, 0).await?;
        }
        Ok(())
    }

    async fn abort_cache_update(&mut self, message: &'static str) {
        if let Some(conn) = self.api_cache_redis.as_mut()
            && let Err(err) = abort_event_update(conn, self.server, self.event_id).await
        {
            tracing::warn!(%err, "{message}");
        }
    }

    async fn finish_cache_update(&mut self, message: &'static str) {
        if let Some(conn) = self.api_cache_redis.as_mut()
            && let Err(err) = finish_event_update(conn, self.server, self.event_id).await
        {
            tracing::warn!(%err, "{message}");
        }
    }

    async fn handle_ranking_data(&mut self) -> Result<HandledRankingData, TrackerError> {
        let now = Utc::now().timestamp();
        if !should_refresh_after_end(
            self.last_border_fetch_at,
            now,
            self.tuning.border_fetch_interval_secs,
        ) {
            return self.handle_top100_only().await;
        }

        let (top100, (border_hash, border)): (
            Top100RankingResponse,
            ([u8; 32], BorderRankingResponse),
        ) = tokio::try_join!(
            self.api.get_top100(self.server, self.event_id),
            self.api.get_border(self.server, self.event_id)
        )?;
        self.last_border_fetch_at = Some(now);

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

        let cache_key = format!("{}-event-{}-main-border", self.server, self.event_id);
        let is_cached = match self.last_border_hash {
            Some(prev) => prev == border_hash,
            // Fresh process: fall back to Redis once to resume across
            // restarts, then mirror the answer locally.
            None => match check_cache(&mut self.redis, &cache_key, &border_hash).await {
                Ok(true) => {
                    self.last_border_hash = Some(border_hash);
                    true
                }
                Ok(false) => false,
                Err(err) => {
                    tracing::warn!(%err, "border cache check failed; treating as miss");
                    false
                }
            },
        };

        let (rankings, border_cache) = if is_cached {
            (main_top100, None)
        } else {
            (
                merge_rankings(main_top100, main_border),
                Some((cache_key, border_hash)),
            )
        };

        Ok(HandledRankingData {
            record_time,
            rankings,
            world_bloom_rankings,
            border_cache,
        })
    }

    /// Between throttled border fetches: track top-100 only. Border ranks
    /// keep their previous state, so the diff simply produces no rows for
    /// them; `border_cache` stays `None` so the stored hash is untouched.
    async fn handle_top100_only(&mut self) -> Result<HandledRankingData, TrackerError> {
        let top100 = self.api.get_top100(self.server, self.event_id).await?;
        let record_time = Utc::now().timestamp();
        let world_bloom_rankings = if self.event_type == SekaiEventType::WorldBloom {
            extract_world_bloom_rankings(
                top100.user_world_bloom_chapter_rankings,
                Vec::new(),
                &self.world_bloom_statuses,
                &self.is_world_bloom_chapter_ended,
            )
        } else {
            HashMap::new()
        };
        Ok(HandledRankingData {
            record_time,
            rankings: top100.rankings,
            world_bloom_rankings,
            border_cache: None,
        })
    }
}

fn collect_visible_user_records(
    record_time: i64,
    data: &HandledRankingData,
) -> Vec<PlayerEventRankingRecordSchema> {
    let refs: Vec<&PlayerRankingSchema> = data.rankings.iter().collect();
    let mut out = build_event_records(record_time, &refs);
    out.extend(
        build_world_bloom_rows(record_time, &data.world_bloom_rankings, |_, _, _, _| false)
            .into_iter()
            .map(|row| row.base),
    );
    out
}

fn should_refresh_after_end(last: Option<i64>, now: i64, interval_secs: u64) -> bool {
    if interval_secs == 0 {
        return true;
    }
    let Ok(interval) = i64::try_from(interval_secs) else {
        return false;
    };
    last.is_none_or(|last| now.saturating_sub(last) >= interval)
}

fn should_write_status_heartbeat(
    last: Option<(i64, i16)>,
    now: i64,
    status: i16,
    interval_secs: u64,
) -> bool {
    match last {
        None => true,
        Some((_, prev_status)) if prev_status != status => true,
        Some((written_at, _)) => should_refresh_after_end(Some(written_at), now, interval_secs),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sea_orm::{Database, DatabaseBackend};
    use std::sync::atomic::{AtomicI64, Ordering};

    static NEXT_EVENT_ID: AtomicI64 = AtomicI64::new(950_000);

    fn ranking(rank: i64, user_id: i64, score: i64) -> PlayerRankingSchema {
        PlayerRankingSchema {
            is_own: None,
            name: Some(format!("player-{user_id}")),
            rank: Some(rank),
            score: Some(score),
            user_id: Some(user_id),
            user_card: None,
            user_profile: None,
            user_profile_honors: Vec::new(),
            user_cheerful_carnival: None,
            user_honor_missions: Vec::new(),
            user_player_frames: Vec::new(),
        }
    }

    pub(crate) async fn tracker_fixture(event_type: SekaiEventType) -> Option<EventTrackerBase> {
        let Ok(redis_url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
            return None;
        };
        let redis_client = redis::Client::open(redis_url).expect("coverage Redis URL is valid");
        let redis = redis::aio::ConnectionManager::new(redis_client)
            .await
            .expect("coverage Redis is reachable");
        let sql = Database::connect("sqlite::memory:").await.unwrap();
        let db = Arc::new(DatabaseEngine::from_connection(
            sql,
            DatabaseBackend::Sqlite,
        ));
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let event_id = millis * 1_000 + NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed) % 1_000;
        create_event_tables(
            &db,
            SekaiServerRegion::Jp,
            event_id,
            event_type == SekaiEventType::WorldBloom,
        )
        .await
        .unwrap();

        Some(EventTrackerBase::new(
            SekaiServerRegion::Jp,
            event_id,
            event_type,
            false,
            db,
            redis.clone(),
            Some(redis),
            HarukiSekaiAPIClient::new("http://127.0.0.1", "").unwrap(),
            UidAnonymizer::disabled(),
            TrackerTuning::default(),
            HashMap::new(),
        ))
    }

    #[test]
    fn post_end_refresh_gate_respects_interval() {
        assert!(should_refresh_after_end(None, 100, 3600));
        assert!(!should_refresh_after_end(Some(100), 200, 3600));
        assert!(should_refresh_after_end(Some(100), 3700, 3600));
        assert!(should_refresh_after_end(Some(100), 101, 0));
    }

    #[test]
    fn status_heartbeat_gate_throttles_repeats_but_not_transitions() {
        // First heartbeat always writes.
        assert!(should_write_status_heartbeat(None, 100, 0, 30));
        // Steady-state repeat inside the interval is suppressed.
        assert!(!should_write_status_heartbeat(Some((100, 0)), 101, 0, 30));
        assert!(should_write_status_heartbeat(Some((100, 0)), 130, 0, 30));
        // A status transition writes immediately in both directions.
        assert!(should_write_status_heartbeat(Some((100, 0)), 101, 1, 30));
        assert!(should_write_status_heartbeat(Some((100, 1)), 101, 0, 30));
        // interval 0 restores write-every-tick.
        assert!(should_write_status_heartbeat(Some((100, 0)), 101, 0, 0));
    }

    #[tokio::test]
    async fn builds_main_and_world_bloom_records_from_tracker_state() {
        let Some(mut tracker) = tracker_fixture(SekaiEventType::WorldBloom).await else {
            return;
        };
        let mut data = HandledRankingData {
            record_time: 1_700_000_000,
            rankings: vec![ranking(1, 100, 1_000)],
            world_bloom_rankings: HashMap::from([(39, vec![ranking(2, 200, 900)])]),
            border_cache: None,
        };

        let (changed, main) = tracker.build_main_records(&data, false);
        assert_eq!(changed.len(), 1);
        assert_eq!(main.len(), 1);
        assert_eq!(main[0].user_id, "100");
        assert!(tracker.build_main_records(&data, true).1.is_empty());

        let world_bloom = tracker.build_world_bloom_records(&data);
        assert_eq!(world_bloom.len(), 1);
        tracker.wl_user_keys.insert(200, 7);
        tracker.prev_world_bloom_state.insert(
            WorldBloomKey {
                user_id_key: 7,
                character_id: 39,
            },
            PlayerState {
                score: 900,
                rank: 2,
            },
        );
        assert!(tracker.build_world_bloom_records(&data).is_empty());

        data.rankings.clear();
        assert!(tracker.build_main_records(&data, false).1.is_empty());
    }

    #[tokio::test]
    async fn persists_main_rankings_and_invalidates_cache_epoch() {
        let Some(mut tracker) = tracker_fixture(SekaiEventType::Marathon).await else {
            return;
        };
        let data = HandledRankingData {
            record_time: 1_700_000_001,
            rankings: vec![ranking(1, 300, 2_000)],
            world_bloom_rankings: HashMap::new(),
            border_cache: Some(("coverage-border-cache".into(), [7; 32])),
        };

        assert!(
            tracker
                .persist_ranking_data(&data, false, false, false, data.record_time)
                .await
                .unwrap()
        );
        assert_eq!(tracker.prev_rank_state[&1].score, 2_000);
        assert_eq!(tracker.last_border_hash, Some([7; 32]));

        let unchanged = HandledRankingData {
            border_cache: None,
            ..data
        };
        assert!(
            !tracker
                .persist_ranking_data(&unchanged, false, false, false, unchanged.record_time)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn coalesces_samples_and_flushes_on_force_or_hot_rank() {
        let Some(mut tracker) = tracker_fixture(SekaiEventType::Marathon).await else {
            return;
        };
        tracker.tuning.flush_interval_secs = 300;
        let t = 1_700_000_100;
        let sample = |ts: i64, score: i64| HandledRankingData {
            record_time: ts,
            rankings: vec![ranking(50, 300, score)],
            world_bloom_rankings: HashMap::new(),
            border_cache: None,
        };

        // Two changed samples accumulate without writing; an identical
        // sample in between adds nothing (baseline advanced at sample time).
        assert!(
            !tracker
                .persist_ranking_data(&sample(t, 1_000), false, false, false, t)
                .await
                .unwrap()
        );
        assert!(
            !tracker
                .persist_ranking_data(&sample(t + 1, 1_000), false, false, false, t + 1)
                .await
                .unwrap()
        );
        assert!(
            !tracker
                .persist_ranking_data(&sample(t + 2, 1_100), false, false, false, t + 2)
                .await
                .unwrap()
        );
        assert_eq!(tracker.pending_records.len(), 2);
        assert_eq!(tracker.pending_since, Some(t));
        assert_eq!(tracker.prev_rank_state[&50].score, 1_100);

        // Forced flush writes both buffered rows and resets the window.
        assert!(
            tracker
                .persist_ranking_data(&sample(t + 3, 1_100), false, true, false, t + 3)
                .await
                .unwrap()
        );
        assert!(tracker.pending_records.is_empty());
        assert_eq!(tracker.pending_since, None);

        // A change touching a hot rank flushes without waiting.
        tracker.tuning.flush_hot_ranks = 10;
        let hot = HandledRankingData {
            record_time: t + 4,
            rankings: vec![ranking(1, 400, 9_000)],
            world_bloom_rankings: HashMap::new(),
            border_cache: None,
        };
        assert!(
            tracker
                .persist_ranking_data(&hot, false, false, false, t + 4)
                .await
                .unwrap()
        );
        assert!(!tracker.pending_hot);
        assert!(tracker.pending_records.is_empty());
    }

    #[tokio::test]
    async fn persists_world_bloom_rankings_and_updates_local_state() {
        let Some(mut tracker) = tracker_fixture(SekaiEventType::WorldBloom).await else {
            return;
        };
        let data = HandledRankingData {
            record_time: 1_700_000_002,
            rankings: Vec::new(),
            world_bloom_rankings: HashMap::from([(21, vec![ranking(5, 400, 1_500)])]),
            border_cache: None,
        };

        assert!(
            tracker
                .persist_ranking_data(&data, true, false, false, data.record_time)
                .await
                .unwrap()
        );
        assert_eq!(tracker.wl_user_keys.len(), 1);
        assert_eq!(tracker.prev_world_bloom_state.len(), 1);
        assert_eq!(tracker.wl_sample_state.len(), 1);

        tracker.begin_cache_update(false).await;
        tracker
            .abort_cache_update("coverage abort should succeed")
            .await;
        tracker
            .finish_cache_update("coverage finish should succeed")
            .await;
    }
}
