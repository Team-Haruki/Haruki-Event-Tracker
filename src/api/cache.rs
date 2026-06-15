use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, Notify};
use tokio::time;

use crate::api::error::ApiError;
use crate::api::stats::{CACHE_STATS, incr};
use crate::config::ApiCacheConfig;

const DIRTY_TTL_SECS: u64 = 300;
const READ_SCRIPT: &str = r#"
local epoch = redis.call('GET', KEYS[1])
if not epoch then
  epoch = 0
else
  epoch = tonumber(epoch)
end
local dirty = redis.call('EXISTS', KEYS[2])
if dirty == 1 then
  return {epoch, 1, '', 0, 0}
end
local value_key = ARGV[1] .. ':v' .. epoch .. ':' .. ARGV[2]
local value = redis.call('GET', value_key)
if value then
  return {epoch, 0, value, 0, 1}
end
local negative = redis.call('EXISTS', value_key .. ':not_found')
return {epoch, 0, '', negative, 0}
"#;
const WRITE_SCRIPT: &str = r#"
local current = redis.call('GET', KEYS[1])
if not current then
  current = 0
else
  current = tonumber(current)
end
if current ~= tonumber(ARGV[1]) then
  return 0
end
if redis.call('EXISTS', KEYS[2]) == 1 then
  return 0
end
redis.call('SETEX', KEYS[3], tonumber(ARGV[3]), ARGV[2])
return 1
"#;

#[derive(Clone)]
pub struct ApiCache {
    conns: Arc<CacheConnections>,
    cfg: ApiCacheConfig,
    l1: L1Cache,
    singleflight: SingleFlight,
}

#[derive(Clone, Copy)]
pub enum CacheTtl {
    LatestRank,
    TraceRank,
    BatchTraceRank,
    UserData,
}

impl ApiCache {
    pub fn new(conns: Vec<ConnectionManager>, cfg: ApiCacheConfig) -> Self {
        Self {
            conns: Arc::new(CacheConnections::new(conns)),
            l1: L1Cache::new(cfg.local_max_entries),
            cfg,
            singleflight: SingleFlight::default(),
        }
    }

    pub fn ttl(&self, ttl: CacheTtl) -> u64 {
        let endpoint_ttl = match ttl {
            CacheTtl::LatestRank => self.cfg.latest_rank_ttl_secs,
            CacheTtl::TraceRank => self.cfg.trace_rank_ttl_secs,
            CacheTtl::BatchTraceRank => self.cfg.batch_trace_rank_ttl_secs,
            CacheTtl::UserData => self.cfg.user_data_ttl_secs,
        };
        if endpoint_ttl == 0 {
            self.cfg.default_ttl_secs
        } else {
            endpoint_ttl
        }
    }

    #[tracing::instrument(skip(self, fetch), fields(server, event_id, suffix))]
    pub async fn get_or_fetch<T, Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
    ) -> Result<T, ApiError>
    where
        T: Serialize + DeserializeOwned,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        if ttl_secs == 0 {
            return fetch.await;
        }

        let control_key = control_cache_key(server, event_id);
        if let Some(control) = self.l1.get_control(&control_key).await {
            incr(&CACHE_STATS.l1_control_hit);
            if control.dirty {
                incr(&CACHE_STATS.dirty_bypass);
                tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
                return self
                    .fetch_with_singleflight(
                        dirty_flight_key(server, event_id, control.epoch, &suffix),
                        fetch,
                        None,
                    )
                    .await;
            }
            let key = value_key(server, event_id, control.epoch, &suffix);
            if let Some(bytes) = self.l1.get_value(&key).await {
                match sonic_rs::from_slice::<T>(&bytes) {
                    Ok(value) => {
                        incr(&CACHE_STATS.l1_hit);
                        tracing::debug!(cache_status = "l1_hit", "api cache L1 hit");
                        return Ok(value);
                    }
                    Err(err) => tracing::warn!(%err, "api cache L1 decode failed"),
                }
            }
            match self.read_l2_value(&key).await {
                Ok(L2ValueRead::Hit(bytes)) => match sonic_rs::from_slice::<T>(&bytes) {
                    Ok(value) => {
                        incr(&CACHE_STATS.l2_hit);
                        tracing::debug!(cache_status = "l2_hit", "api cache L2 hit");
                        self.store_l1_value(key, bytes).await;
                        return Ok(value);
                    }
                    Err(err) => tracing::warn!(%err, "api cache decode failed"),
                },
                Ok(L2ValueRead::NotFound) => {
                    incr(&CACHE_STATS.l2_not_found);
                    tracing::debug!(cache_status = "l2_not_found", "api cache negative hit");
                    return Err(ApiError::NotFound);
                }
                Ok(L2ValueRead::Miss) => {
                    incr(&CACHE_STATS.l2_miss);
                }
                Err(err) => {
                    incr(&CACHE_STATS.l2_timeout);
                    tracing::warn!(%err, "api cache value read failed");
                    return fetch.await;
                }
            }
            tracing::debug!(cache_status = "l2_miss", "api cache miss");
            return self
                .fetch_with_singleflight(
                    key.clone(),
                    fetch,
                    Some(CacheWriteContext {
                        server: server.to_owned(),
                        event_id,
                        epoch: control.epoch,
                        value_key: key,
                        negative_key: negative_key(server, event_id, control.epoch, &suffix),
                        ttl_secs,
                    }),
                )
                .await;
        }

        match self.read_l2_combined(server, event_id, &suffix).await {
            Ok(L2CombinedRead::Dirty { epoch }) => {
                self.store_l1_control(control_key, epoch, true).await;
                incr(&CACHE_STATS.dirty_bypass);
                tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
                return self
                    .fetch_with_singleflight(
                        dirty_flight_key(server, event_id, epoch, &suffix),
                        fetch,
                        None,
                    )
                    .await;
            }
            Ok(L2CombinedRead::Hit { epoch, key, bytes }) => {
                match sonic_rs::from_slice::<T>(&bytes) {
                    Ok(value) => {
                        self.store_l1_control(control_key, epoch, false).await;
                        self.store_l1_value(key, bytes).await;
                        incr(&CACHE_STATS.l2_hit);
                        tracing::debug!(cache_status = "l2_hit", "api cache L2 hit");
                        return Ok(value);
                    }
                    Err(err) => tracing::warn!(%err, "api cache decode failed"),
                }
            }
            Ok(L2CombinedRead::NotFound { epoch }) => {
                self.store_l1_control(control_key, epoch, false).await;
                incr(&CACHE_STATS.l2_not_found);
                tracing::debug!(cache_status = "l2_not_found", "api cache negative hit");
                return Err(ApiError::NotFound);
            }
            Ok(L2CombinedRead::Miss { epoch, key }) => {
                self.store_l1_control(control_key, epoch, false).await;
                incr(&CACHE_STATS.l2_miss);
                tracing::debug!(cache_status = "l2_miss", "api cache miss");
                return self
                    .fetch_with_singleflight(
                        key.clone(),
                        fetch,
                        Some(CacheWriteContext {
                            server: server.to_owned(),
                            event_id,
                            epoch,
                            value_key: key,
                            negative_key: negative_key(server, event_id, epoch, &suffix),
                            ttl_secs,
                        }),
                    )
                    .await;
            }
            Err(err) => {
                incr(&CACHE_STATS.l2_timeout);
                tracing::warn!(%err, "api cache combined read failed");
            }
        }

        self.fetch_with_singleflight(fallback_flight_key(server, event_id, &suffix), fetch, None)
            .await
    }

    async fn fetch_with_singleflight<T, Fut>(
        &self,
        flight_key: String,
        fetch: Fut,
        write_context: Option<CacheWriteContext>,
    ) -> Result<T, ApiError>
    where
        T: Serialize + DeserializeOwned,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        match self.singleflight.begin(flight_key.clone()).await {
            Flight::Waiter(entry) => {
                incr(&CACHE_STATS.singleflight_wait);
                tracing::debug!(
                    cache_status = "singleflight_wait",
                    "api cache waiting for in-flight fetch"
                );
                if let Some(value) = SingleFlight::wait::<T>(entry).await {
                    return value;
                }
                tracing::debug!(
                    cache_status = "singleflight_retry",
                    "api cache in-flight fetch was not shareable"
                );
                fetch.await
            }
            Flight::Owner(entry) => {
                let result = self.fetch_and_maybe_cache(fetch, write_context).await;
                let shared = shared_fetch_result(&result);
                self.singleflight.finish(&flight_key, entry, shared).await;
                result
            }
        }
    }

    async fn fetch_and_maybe_cache<T, Fut>(
        &self,
        fetch: Fut,
        write_context: Option<CacheWriteContext>,
    ) -> Result<T, ApiError>
    where
        T: Serialize + DeserializeOwned,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let result = fetch.await;
        if matches!(result, Err(ApiError::NotFound)) {
            if let Some(ctx) = write_context
                && self.cfg.negative_ttl_secs > 0
            {
                match self
                    .write_l2_if_clean(
                        &ctx,
                        &ctx.negative_key,
                        b"1".to_vec(),
                        self.cfg.negative_ttl_secs,
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {}
                    Err(err) => tracing::warn!(%err, "api cache negative write failed"),
                }
            }
            return result;
        }

        let value = result?;
        let bytes = match sonic_rs::to_vec(&value) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(%err, "api cache encode failed");
                return Ok(value);
            }
        };
        if bytes.len() > self.cfg.max_value_bytes {
            tracing::debug!(
                cache_status = "too_large",
                bytes = bytes.len(),
                max = self.cfg.max_value_bytes,
                "api cache value too large"
            );
            return Ok(value);
        }

        let Some(ctx) = write_context else {
            return Ok(value);
        };
        match self
            .write_l2_if_clean(&ctx, &ctx.value_key, bytes.clone(), ctx.ttl_secs)
            .await
        {
            Ok(true) => self.store_l1_value(ctx.value_key.clone(), bytes).await,
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, "api cache write failed"),
        }
        Ok(value)
    }

    async fn read_l2_combined(
        &self,
        server: &str,
        event_id: i64,
        suffix: &str,
    ) -> Result<L2CombinedRead, redis::RedisError> {
        let mut conn = self.conns.connection();
        let base = base_key(server, event_id);
        let script = redis::Script::new(READ_SCRIPT);
        script
            .key(epoch_key(server, event_id))
            .key(dirty_key(server, event_id))
            .arg(base)
            .arg(suffix);
        let fut = script.invoke_async::<_, (i64, i64, Vec<u8>, i64, i64)>(&mut conn);
        let (epoch, dirty, value, negative, has_value) = self.with_timeout(fut).await?;
        if dirty != 0 {
            return Ok(L2CombinedRead::Dirty { epoch });
        }
        let key = value_key(server, event_id, epoch, suffix);
        if has_value != 0 {
            return Ok(L2CombinedRead::Hit {
                epoch,
                key,
                bytes: value,
            });
        }
        if negative != 0 {
            return Ok(L2CombinedRead::NotFound { epoch });
        }
        Ok(L2CombinedRead::Miss { epoch, key })
    }

    async fn read_l2_value(&self, key: &str) -> Result<L2ValueRead, redis::RedisError> {
        let mut conn = self.conns.connection();
        let mut pipe = redis::pipe();
        pipe.get(key).exists(format!("{key}:not_found"));
        let fut = pipe.query_async::<(Option<Vec<u8>>, bool)>(&mut conn);
        let (value, negative) = self.with_timeout(fut).await?;
        if let Some(bytes) = value {
            Ok(L2ValueRead::Hit(bytes))
        } else if negative {
            Ok(L2ValueRead::NotFound)
        } else {
            Ok(L2ValueRead::Miss)
        }
    }

    async fn write_l2_if_clean(
        &self,
        ctx: &CacheWriteContext,
        key: &str,
        bytes: Vec<u8>,
        ttl_secs: u64,
    ) -> Result<bool, redis::RedisError> {
        let mut conn = self.conns.connection();
        let script = redis::Script::new(WRITE_SCRIPT);
        script
            .key(epoch_key(&ctx.server, ctx.event_id))
            .key(dirty_key(&ctx.server, ctx.event_id))
            .key(key)
            .arg(ctx.epoch)
            .arg(bytes)
            .arg(ttl_secs);
        let fut = script.invoke_async::<_, i64>(&mut conn);
        self.with_timeout(fut).await.map(|written| written != 0)
    }

    async fn with_timeout<F, T>(&self, fut: F) -> Result<T, redis::RedisError>
    where
        F: Future<Output = Result<T, redis::RedisError>>,
    {
        if self.cfg.command_timeout_ms == 0 {
            return fut.await;
        }
        time::timeout(Duration::from_millis(self.cfg.command_timeout_ms), fut)
            .await
            .map_err(|_| {
                redis::RedisError::from((redis::ErrorKind::Io, "api cache command timed out"))
            })?
    }

    async fn store_l1_control(&self, key: String, epoch: i64, dirty: bool) {
        if self.cfg.local_control_ttl_ms == 0 {
            return;
        }
        self.l1
            .insert_control(
                key,
                L1Control {
                    epoch,
                    dirty,
                    expires_at: Instant::now()
                        + Duration::from_millis(self.cfg.local_control_ttl_ms),
                },
            )
            .await;
    }

    async fn store_l1_value(&self, key: String, bytes: Vec<u8>) {
        if self.cfg.local_value_ttl_ms == 0 {
            return;
        }
        self.l1
            .insert_value(
                key,
                L1Value {
                    bytes,
                    expires_at: Instant::now() + Duration::from_millis(self.cfg.local_value_ttl_ms),
                },
            )
            .await;
    }
}

struct CacheConnections {
    conns: Vec<ConnectionManager>,
    next: AtomicUsize,
}

impl CacheConnections {
    fn new(conns: Vec<ConnectionManager>) -> Self {
        assert!(
            !conns.is_empty(),
            "api cache requires at least one Redis connection"
        );
        Self {
            conns,
            next: AtomicUsize::new(0),
        }
    }

    fn connection(&self) -> ConnectionManager {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        self.conns[idx].clone()
    }
}

#[derive(Clone, Default)]
struct L1Cache {
    max_entries: usize,
    inner: Arc<Mutex<L1Inner>>,
}

#[derive(Default)]
struct L1Inner {
    controls: HashMap<String, L1Control>,
    values: HashMap<String, L1Value>,
}

#[derive(Clone, Copy)]
struct L1Control {
    epoch: i64,
    dirty: bool,
    expires_at: Instant,
}

#[derive(Clone)]
struct L1Value {
    bytes: Vec<u8>,
    expires_at: Instant,
}

impl L1Cache {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            inner: Arc::new(Mutex::new(L1Inner::default())),
        }
    }

    async fn get_control(&self, key: &str) -> Option<L1Control> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        match inner.controls.get(key).copied() {
            Some(control) if control.expires_at > now => Some(control),
            Some(_) => {
                inner.controls.remove(key);
                None
            }
            None => None,
        }
    }

    async fn insert_control(&self, key: String, value: L1Control) {
        if self.max_entries == 0 {
            return;
        }
        let mut inner = self.inner.lock().await;
        prune_if_full(self.max_entries, &mut inner.controls);
        inner.controls.insert(key, value);
    }

    async fn get_value(&self, key: &str) -> Option<Vec<u8>> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        match inner.values.get(key) {
            Some(value) if value.expires_at > now => Some(value.bytes.clone()),
            Some(_) => {
                inner.values.remove(key);
                None
            }
            None => None,
        }
    }

    async fn insert_value(&self, key: String, value: L1Value) {
        if self.max_entries == 0 {
            return;
        }
        let mut inner = self.inner.lock().await;
        prune_if_full(self.max_entries, &mut inner.values);
        inner.values.insert(key, value);
    }
}

fn prune_if_full<T>(max_entries: usize, values: &mut HashMap<String, T>) {
    if values.len() < max_entries {
        return;
    }
    values.clear();
}

enum L2CombinedRead {
    Dirty {
        epoch: i64,
    },
    Hit {
        epoch: i64,
        key: String,
        bytes: Vec<u8>,
    },
    NotFound {
        epoch: i64,
    },
    Miss {
        epoch: i64,
        key: String,
    },
}

enum L2ValueRead {
    Hit(Vec<u8>),
    NotFound,
    Miss,
}

#[derive(Clone)]
struct CacheWriteContext {
    server: String,
    event_id: i64,
    epoch: i64,
    value_key: String,
    negative_key: String,
    ttl_secs: u64,
}

fn shared_fetch_result<T>(result: &Result<T, ApiError>) -> Option<SharedFetchResult>
where
    T: Serialize,
{
    match result {
        Ok(value) => match sonic_rs::to_vec(value) {
            Ok(bytes) => Some(SharedFetchResult::Value(bytes)),
            Err(err) => {
                tracing::warn!(%err, "api cache singleflight encode failed");
                None
            }
        },
        Err(ApiError::NotFound) => Some(SharedFetchResult::NotFound),
        Err(_) => None,
    }
}

#[derive(Clone, Default)]
struct SingleFlight {
    inner: Arc<Mutex<HashMap<String, Arc<InFlightEntry>>>>,
}

struct InFlightEntry {
    notify: Notify,
    state: Mutex<InFlightState>,
}

#[derive(Default)]
struct InFlightState {
    done: bool,
    result: Option<SharedFetchResult>,
}

enum Flight {
    Owner(Arc<InFlightEntry>),
    Waiter(Arc<InFlightEntry>),
}

enum SharedFetchResult {
    Value(Vec<u8>),
    NotFound,
}

impl SingleFlight {
    async fn begin(&self, key: String) -> Flight {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get(&key) {
            return Flight::Waiter(entry.clone());
        }
        let entry = Arc::new(InFlightEntry {
            notify: Notify::new(),
            state: Mutex::new(InFlightState::default()),
        });
        inner.insert(key, entry.clone());
        Flight::Owner(entry)
    }

    async fn finish(
        &self,
        key: &str,
        entry: Arc<InFlightEntry>,
        result: Option<SharedFetchResult>,
    ) {
        {
            let mut state = entry.state.lock().await;
            state.done = true;
            state.result = result;
        }
        let mut inner = self.inner.lock().await;
        if inner
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, &entry))
        {
            inner.remove(key);
        }
        drop(inner);
        entry.notify.notify_waiters();
    }

    async fn wait<T>(entry: Arc<InFlightEntry>) -> Option<Result<T, ApiError>>
    where
        T: DeserializeOwned,
    {
        loop {
            let notified = entry.notify.notified();
            {
                let state = entry.state.lock().await;
                if state.done {
                    return match &state.result {
                        Some(SharedFetchResult::Value(bytes)) => {
                            match sonic_rs::from_slice::<T>(bytes) {
                                Ok(value) => Some(Ok(value)),
                                Err(err) => {
                                    tracing::warn!(%err, "api cache singleflight decode failed");
                                    None
                                }
                            }
                        }
                        Some(SharedFetchResult::NotFound) => Some(Err(ApiError::NotFound)),
                        None => None,
                    };
                }
            }
            notified.await;
        }
    }
}

pub async fn begin_event_update(
    conn: &mut ConnectionManager,
    server: impl std::fmt::Display,
    event_id: i64,
) -> Result<(), redis::RedisError> {
    conn.set_ex::<_, _, ()>(
        dirty_key(&server.to_string(), event_id),
        "1",
        DIRTY_TTL_SECS,
    )
    .await
}

pub async fn finish_event_update(
    conn: &mut ConnectionManager,
    server: impl std::fmt::Display,
    event_id: i64,
) -> Result<(), redis::RedisError> {
    let server = server.to_string();
    let mut pipe = redis::pipe();
    pipe.incr(epoch_key(&server, event_id), 1)
        .ignore()
        .del(dirty_key(&server, event_id))
        .ignore();
    pipe.query_async::<()>(conn).await
}

pub async fn abort_event_update(
    conn: &mut ConnectionManager,
    server: impl std::fmt::Display,
    event_id: i64,
) -> Result<(), redis::RedisError> {
    conn.del::<_, ()>(dirty_key(&server.to_string(), event_id))
        .await
}

fn value_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!(
        "{base}:v{epoch}:{suffix}",
        base = base_key(server, event_id)
    )
}

fn negative_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!("{}:not_found", value_key(server, event_id, epoch, suffix))
}

fn dirty_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!(
        "{base}:dirty:v{epoch}:{suffix}",
        base = base_key(server, event_id)
    )
}

fn fallback_flight_key(server: &str, event_id: i64, suffix: &str) -> String {
    format!(
        "{base}:fallback:{suffix}",
        base = base_key(server, event_id)
    )
}

fn control_cache_key(server: &str, event_id: i64) -> String {
    format!("{base}:control", base = base_key(server, event_id))
}

fn epoch_key(server: &str, event_id: i64) -> String {
    format!("{base}:epoch", base = base_key(server, event_id))
}

fn dirty_key(server: &str, event_id: i64) -> String {
    format!("{base}:dirty", base = base_key(server, event_id))
}

fn base_key(server: &str, event_id: i64) -> String {
    let server = server.to_ascii_lowercase();
    format!("haruki:tracker:{server}:{event_id}:api_cache")
}

pub fn rank_suffix(kind: &str, rank: i64) -> String {
    format!("{kind}:rank:{rank}")
}

pub fn user_suffix(kind: &str, user_id: &str) -> String {
    format!("{kind}:user:{user_id}")
}

pub fn wb_rank_suffix(kind: &str, character_id: i64, rank: i64) -> String {
    format!("wb:{character_id}:{kind}:rank:{rank}")
}

pub fn batch_rank_suffix(kind: &str, ranks: &[i64]) -> String {
    let ranks = ranks
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("{kind}:ranks:{ranks}")
}

pub fn wb_batch_rank_suffix(kind: &str, character_id: i64, ranks: &[i64]) -> String {
    let ranks = ranks
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("wb:{character_id}:{kind}:ranks:{ranks}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_epoch_value_keys() {
        assert_eq!(
            value_key("JP", 137, 3, "trace:rank:100"),
            "haruki:tracker:jp:137:api_cache:v3:trace:rank:100"
        );
        assert_eq!(
            negative_key("JP", 137, 3, "trace:rank:100"),
            "haruki:tracker:jp:137:api_cache:v3:trace:rank:100:not_found"
        );
        assert_eq!(
            dirty_flight_key("JP", 137, 3, "trace:rank:100"),
            "haruki:tracker:jp:137:api_cache:dirty:v3:trace:rank:100"
        );
        assert_eq!(
            fallback_flight_key("JP", 137, "trace:rank:100"),
            "haruki:tracker:jp:137:api_cache:fallback:trace:rank:100"
        );
        assert_eq!(
            control_cache_key("JP", 137),
            "haruki:tracker:jp:137:api_cache:control"
        );
        assert_eq!(
            dirty_key("en", 200),
            "haruki:tracker:en:200:api_cache:dirty"
        );
    }

    #[tokio::test]
    async fn l1_value_hit_returns_cached_bytes() {
        let l1 = L1Cache::new(16);
        l1.insert_value(
            "value".to_owned(),
            L1Value {
                bytes: b"cached".to_vec(),
                expires_at: Instant::now() + Duration::from_secs(1),
            },
        )
        .await;

        assert_eq!(l1.get_value("value").await, Some(b"cached".to_vec()));
    }

    #[tokio::test]
    async fn l1_value_expiry_removes_cached_bytes() {
        let l1 = L1Cache::new(16);
        l1.insert_value(
            "value".to_owned(),
            L1Value {
                bytes: b"stale".to_vec(),
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        )
        .await;

        assert_eq!(l1.get_value("value").await, None);
        assert_eq!(l1.get_value("value").await, None);
    }

    #[tokio::test]
    async fn l1_control_tracks_epoch_and_dirty_state() {
        let l1 = L1Cache::new(16);
        l1.insert_control(
            "control".to_owned(),
            L1Control {
                epoch: 7,
                dirty: true,
                expires_at: Instant::now() + Duration::from_secs(1),
            },
        )
        .await;

        let control = l1.get_control("control").await.unwrap();
        assert_eq!(control.epoch, 7);
        assert!(control.dirty);
    }

    #[tokio::test]
    async fn l1_zero_max_entries_disables_storage() {
        let l1 = L1Cache::new(0);
        l1.insert_value(
            "value".to_owned(),
            L1Value {
                bytes: b"cached".to_vec(),
                expires_at: Instant::now() + Duration::from_secs(1),
            },
        )
        .await;

        assert_eq!(l1.get_value("value").await, None);
    }

    #[tokio::test]
    async fn singleflight_shares_success_result() {
        let singleflight = SingleFlight::default();
        let key = "trace:rank:1".to_owned();
        let owner = match singleflight.begin(key.clone()).await {
            Flight::Owner(entry) => entry,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()).await {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };

        let task =
            tokio::spawn(async move { SingleFlight::wait::<i64>(waiter).await.unwrap().unwrap() });
        singleflight
            .finish(
                &key,
                owner,
                Some(SharedFetchResult::Value(sonic_rs::to_vec(&42).unwrap())),
            )
            .await;

        assert_eq!(task.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn singleflight_shares_not_found_result() {
        let singleflight = SingleFlight::default();
        let key = "trace:rank:40000".to_owned();
        let owner = match singleflight.begin(key.clone()).await {
            Flight::Owner(entry) => entry,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()).await {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };

        let task = tokio::spawn(async move { SingleFlight::wait::<i64>(waiter).await.unwrap() });
        singleflight
            .finish(&key, owner, Some(SharedFetchResult::NotFound))
            .await;

        assert!(matches!(task.await.unwrap(), Err(ApiError::NotFound)));
    }
}
