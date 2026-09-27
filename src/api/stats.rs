use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::time;

pub static CACHE_STATS: CacheStats = CacheStats::new();
pub static ACCESS_STATS: AccessStats = AccessStats::new();
pub static API_STATS: ApiStats = ApiStats::new();

static LOGGER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct CacheStats {
    pub l1_hit: AtomicU64,
    pub l1_control_hit: AtomicU64,
    pub l1_expired: AtomicU64,
    pub l1_evicted: AtomicU64,
    /// Gauges refreshed by the L1 sweeper, not per-interval counters.
    pub l1_entries: AtomicU64,
    pub l1_bytes: AtomicU64,
    pub l2_hit: AtomicU64,
    pub l2_miss: AtomicU64,
    pub l2_not_found: AtomicU64,
    pub l2_timeout: AtomicU64,
    pub dirty_bypass: AtomicU64,
    pub lookup_singleflight_wait: AtomicU64,
    pub singleflight_wait: AtomicU64,
    pub batch_l1_hit: AtomicU64,
    pub batch_l2_hit: AtomicU64,
    pub batch_miss: AtomicU64,
    pub batch_too_large: AtomicU64,
    pub batch_singleflight_wait: AtomicU64,
}

impl CacheStats {
    pub const fn new() -> Self {
        Self {
            l1_hit: AtomicU64::new(0),
            l1_control_hit: AtomicU64::new(0),
            l1_expired: AtomicU64::new(0),
            l1_evicted: AtomicU64::new(0),
            l1_entries: AtomicU64::new(0),
            l1_bytes: AtomicU64::new(0),
            l2_hit: AtomicU64::new(0),
            l2_miss: AtomicU64::new(0),
            l2_not_found: AtomicU64::new(0),
            l2_timeout: AtomicU64::new(0),
            dirty_bypass: AtomicU64::new(0),
            lookup_singleflight_wait: AtomicU64::new(0),
            singleflight_wait: AtomicU64::new(0),
            batch_l1_hit: AtomicU64::new(0),
            batch_l2_hit: AtomicU64::new(0),
            batch_miss: AtomicU64::new(0),
            batch_too_large: AtomicU64::new(0),
            batch_singleflight_wait: AtomicU64::new(0),
        }
    }
}

impl Default for CacheStats {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AccessStats {
    pub logged: AtomicU64,
    pub sampled: AtomicU64,
    pub dropped: AtomicU64,
}

impl AccessStats {
    pub const fn new() -> Self {
        Self {
            logged: AtomicU64::new(0),
            sampled: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }
}

impl Default for AccessStats {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ApiStats {
    pub service_unavailable: AtomicU64,
}

impl ApiStats {
    pub const fn new() -> Self {
        Self {
            service_unavailable: AtomicU64::new(0),
        }
    }
}

impl Default for ApiStats {
    fn default() -> Self {
        Self::new()
    }
}

pub fn incr(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

pub fn set(gauge: &AtomicU64, value: u64) {
    gauge.store(value, Ordering::Relaxed);
}

pub fn spawn_aggregation_logger() {
    if LOGGER_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    tokio::spawn(async {
        loop {
            time::sleep(Duration::from_secs(60)).await;
            log_snapshot();
        }
    });
}

fn take(counter: &AtomicU64) -> u64 {
    counter.swap(0, Ordering::Relaxed)
}

fn log_snapshot() {
    let (main_log_dropped, access_log_dropped) = crate::logger::file_sink_dropped_lines();
    tracing::info!(
        target: "api_stats",
        l1_hit = take(&CACHE_STATS.l1_hit),
        l1_control_hit = take(&CACHE_STATS.l1_control_hit),
        l1_expired = take(&CACHE_STATS.l1_expired),
        l1_evicted = take(&CACHE_STATS.l1_evicted),
        l1_entries = CACHE_STATS.l1_entries.load(Ordering::Relaxed),
        l1_bytes = CACHE_STATS.l1_bytes.load(Ordering::Relaxed),
        l2_hit = take(&CACHE_STATS.l2_hit),
        l2_miss = take(&CACHE_STATS.l2_miss),
        l2_not_found = take(&CACHE_STATS.l2_not_found),
        l2_timeout = take(&CACHE_STATS.l2_timeout),
        dirty_bypass = take(&CACHE_STATS.dirty_bypass),
        lookup_singleflight_wait = take(&CACHE_STATS.lookup_singleflight_wait),
        singleflight_wait = take(&CACHE_STATS.singleflight_wait),
        batch_l1_hit = take(&CACHE_STATS.batch_l1_hit),
        batch_l2_hit = take(&CACHE_STATS.batch_l2_hit),
        batch_miss = take(&CACHE_STATS.batch_miss),
        batch_too_large = take(&CACHE_STATS.batch_too_large),
        batch_singleflight_wait = take(&CACHE_STATS.batch_singleflight_wait),
        access_logged = take(&ACCESS_STATS.logged),
        access_sampled = take(&ACCESS_STATS.sampled),
        access_dropped = take(&ACCESS_STATS.dropped),
        service_unavailable = take(&API_STATS.service_unavailable),
        // Cumulative, not per-interval: `ErrorCounter` has no reset API.
        main_log_dropped_total = main_log_dropped,
        access_log_dropped_total = access_log_dropped,
        "api aggregate stats"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_reset_and_default_to_zero() {
        let cache = CacheStats::default();
        let access = AccessStats::default();
        let api = ApiStats::default();
        incr(&cache.l1_hit);
        add(&cache.l1_expired, 3);
        set(&cache.l1_bytes, 42);
        incr(&access.logged);
        incr(&api.service_unavailable);

        assert_eq!(take(&cache.l1_hit), 1);
        assert_eq!(take(&cache.l1_expired), 3);
        assert_eq!(cache.l1_bytes.load(Ordering::Relaxed), 42);
        assert_eq!(take(&cache.l1_hit), 0);
        assert_eq!(take(&access.logged), 1);
        assert_eq!(take(&api.service_unavailable), 1);
    }

    #[tokio::test]
    async fn aggregation_logger_only_starts_once() {
        spawn_aggregation_logger();
        spawn_aggregation_logger();
        log_snapshot();
        assert!(LOGGER_STARTED.load(Ordering::Relaxed));
    }
}
