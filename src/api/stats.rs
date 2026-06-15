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
    pub l2_hit: AtomicU64,
    pub l2_miss: AtomicU64,
    pub l2_not_found: AtomicU64,
    pub l2_timeout: AtomicU64,
    pub dirty_bypass: AtomicU64,
    pub singleflight_wait: AtomicU64,
}

impl CacheStats {
    pub const fn new() -> Self {
        Self {
            l1_hit: AtomicU64::new(0),
            l1_control_hit: AtomicU64::new(0),
            l2_hit: AtomicU64::new(0),
            l2_miss: AtomicU64::new(0),
            l2_not_found: AtomicU64::new(0),
            l2_timeout: AtomicU64::new(0),
            dirty_bypass: AtomicU64::new(0),
            singleflight_wait: AtomicU64::new(0),
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
    tracing::info!(
        target: "api_stats",
        l1_hit = take(&CACHE_STATS.l1_hit),
        l1_control_hit = take(&CACHE_STATS.l1_control_hit),
        l2_hit = take(&CACHE_STATS.l2_hit),
        l2_miss = take(&CACHE_STATS.l2_miss),
        l2_not_found = take(&CACHE_STATS.l2_not_found),
        l2_timeout = take(&CACHE_STATS.l2_timeout),
        dirty_bypass = take(&CACHE_STATS.dirty_bypass),
        singleflight_wait = take(&CACHE_STATS.singleflight_wait),
        access_logged = take(&ACCESS_STATS.logged),
        access_sampled = take(&ACCESS_STATS.sampled),
        access_dropped = take(&ACCESS_STATS.dropped),
        service_unavailable = take(&API_STATS.service_unavailable),
        "api aggregate stats"
    );
}
