use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use quick_cache::sync::{Cache as QuickCache, DefaultLifecycle};
use quick_cache::{DefaultHashBuilder, Lifecycle, OptionsBuilder, UnitWeighter, Weighter};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Notify;
use tokio::time::{self, MissedTickBehavior};

use crate::api::error::ApiError;
use crate::api::stats::{CACHE_STATS, add, incr, set};
use crate::config::ApiCacheConfig;

const DIRTY_TTL_SECS: u64 = 300;
/// Payloads at or above this size are gzipped on a blocking thread so large
/// batch responses don't stall the async worker.
const GZIP_SPAWN_BLOCKING_THRESHOLD: usize = 32 * 1024;
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

static READ_SCRIPT_HANDLE: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(READ_SCRIPT));
static WRITE_SCRIPT_HANDLE: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(WRITE_SCRIPT));

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
    ReplayOverview,
}

#[derive(Clone)]
pub struct CachedJson {
    pub bytes: Bytes,
    pub encoding: CachedJsonEncoding,
    /// The cache epoch these bytes belong to: set only when they were read
    /// from, or accepted into, the epoch-keyed cache while the event was
    /// clean. `None` for dirty bypasses, cache errors and uncacheable sizes.
    pub epoch: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CachedJsonEncoding {
    Identity,
    Gzip,
}

impl CachedJson {
    fn identity(bytes: Bytes) -> Self {
        Self {
            bytes,
            encoding: CachedJsonEncoding::Identity,
            epoch: None,
        }
    }

    fn gzip(bytes: Bytes) -> Self {
        Self {
            bytes,
            encoding: CachedJsonEncoding::Gzip,
            epoch: None,
        }
    }

    fn at_epoch(mut self, epoch: Option<i64>) -> Self {
        self.epoch = epoch;
        self
    }
}

impl ApiCache {
    pub fn new(conns: Vec<ConnectionManager>, cfg: ApiCacheConfig) -> Self {
        let l1 = L1Cache::new(cfg.local_max_entries, cfg.local_max_bytes);
        l1.spawn_sweeper(Duration::from_millis(cfg.local_sweep_interval_ms));
        Self {
            conns: Arc::new(CacheConnections::new(conns)),
            l1,
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
            CacheTtl::ReplayOverview => self.cfg.replay_overview_ttl_secs,
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
        let fetch_bytes = async move {
            let value = fetch.await?;
            encode_json_bytes(&value)
        };
        let bytes = self
            .get_or_fetch_bytes_with_options(
                server,
                event_id,
                suffix,
                ttl_secs,
                fetch_bytes,
                CacheOptions {
                    max_value_bytes: self.cfg.max_value_bytes,
                    is_batch: false,
                    validate_cached_bytes: Some(validate_json_bytes::<T>),
                },
            )
            .await?;
        sonic_rs::from_slice::<T>(&bytes).map_err(|err| {
            tracing::warn!(%err, "api cache decoded invalid JSON bytes");
            ApiError::ServiceUnavailable("api cache decode failed".into())
        })
    }

    #[tracing::instrument(skip(self, fetch), fields(server, event_id, suffix))]
    pub async fn get_or_fetch_static<T, Fut>(
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
        let fetch_bytes = async move {
            let value = fetch.await?;
            encode_json_bytes(&value)
        };
        let bytes = self
            .get_or_fetch_static_bytes(
                server,
                event_id,
                suffix,
                ttl_secs,
                fetch_bytes,
                CacheOptions {
                    max_value_bytes: self.cfg.batch_max_value_bytes,
                    is_batch: false,
                    validate_cached_bytes: Some(validate_json_bytes::<T>),
                },
            )
            .await?;
        sonic_rs::from_slice::<T>(&bytes).map_err(|err| {
            tracing::warn!(%err, "api cache decoded invalid static JSON bytes");
            ApiError::ServiceUnavailable("api cache decode failed".into())
        })
    }

    #[tracing::instrument(skip(self, fetch), fields(server, event_id, suffix))]
    pub async fn get_or_fetch_json_bytes<T, Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        T: Serialize,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let fetch_bytes = async move {
            let value = fetch.await?;
            encode_json_bytes(&value)
        };
        self.get_or_fetch_bytes(server, event_id, suffix, ttl_secs, fetch_bytes)
            .await
    }

    #[tracing::instrument(skip(self, fetch), fields(server, event_id, suffix))]
    pub async fn get_or_fetch_static_json_bytes<T, Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        T: Serialize,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        self.get_or_fetch_static_json_bytes_checked(server, event_id, suffix, ttl_secs, None, fetch)
            .await
    }

    /// Like [`Self::get_or_fetch_static_json_bytes`], treating an L2 value
    /// that fails `validate` as a miss. Callers that consume the bytes
    /// without decoding them into `T` pass a check cheaper than a full
    /// typed decode.
    pub async fn get_or_fetch_static_json_bytes_checked<T, Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        validate: Option<fn(&Bytes) -> bool>,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        T: Serialize,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let fetch_bytes = async move {
            let value = fetch.await?;
            encode_json_bytes(&value)
        };
        self.get_or_fetch_static_bytes(
            server,
            event_id,
            suffix,
            ttl_secs,
            fetch_bytes,
            CacheOptions {
                max_value_bytes: self.cfg.batch_max_value_bytes,
                is_batch: false,
                validate_cached_bytes: validate,
            },
        )
        .await
    }

    #[tracing::instrument(skip(self, fetch), fields(server, event_id, suffix, prefer_gzip))]
    pub async fn get_or_fetch_encoded_json<T, Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        prefer_gzip: bool,
        fetch: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        T: Serialize,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let fetch_bytes = async move {
            let value = fetch.await?;
            encode_json_bytes(&value)
        };
        if !prefer_gzip || !self.cfg.precompress_gzip_enabled {
            return self
                .get_or_fetch_bytes(server, event_id, suffix, ttl_secs, fetch_bytes)
                .await
                .map(CachedJson::identity);
        }
        self.get_or_fetch_precompressed_bytes(server, event_id, suffix, ttl_secs, fetch_bytes)
            .await
    }

    #[tracing::instrument(skip(self, fetch), fields(server, event_id, suffix, prefer_gzip))]
    pub async fn get_or_fetch_batch_encoded_json<Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        prefer_gzip: bool,
        fetch: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        let options = CacheOptions {
            max_value_bytes: self.cfg.batch_max_value_bytes,
            is_batch: true,
            validate_cached_bytes: None,
        };
        if !prefer_gzip || !self.cfg.precompress_gzip_enabled {
            return self
                .get_or_fetch_bytes_with_options(server, event_id, suffix, ttl_secs, fetch, options)
                .await
                .map(CachedJson::identity);
        }
        self.get_or_fetch_precompressed_bytes_with_options(
            server, event_id, suffix, ttl_secs, fetch, options,
        )
        .await
    }

    async fn get_or_fetch_bytes<Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        self.get_or_fetch_bytes_with_options(
            server,
            event_id,
            suffix,
            ttl_secs,
            fetch,
            CacheOptions {
                max_value_bytes: self.cfg.max_value_bytes,
                is_batch: false,
                validate_cached_bytes: None,
            },
        )
        .await
    }

    async fn get_or_fetch_static_bytes<Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
        options: CacheOptions,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        if ttl_secs == 0 {
            return fetch.await;
        }
        let key = static_value_key(server, event_id, &suffix);
        // L1 entries are validated on the way in (fresh encode or checked L2
        // read), so hits skip re-validation — it would re-parse the payload.
        if let Some(bytes) = self.l1.get_value(&key) {
            incr(&CACHE_STATS.l1_hit);
            tracing::debug!(cache_status = "static_l1_hit", "api static cache L1 hit");
            return Ok(bytes);
        }

        self.lookup_with_singleflight(
            static_lookup_flight_key(server, event_id, &suffix),
            options,
            async {
                match self.read_l2_value(&key).await {
                    Ok(L2ValueRead::Hit {
                        bytes,
                        remaining_ms,
                    }) => {
                        if cached_bytes_are_valid(options, &bytes) {
                            incr(&CACHE_STATS.l2_hit);
                            tracing::debug!(
                                cache_status = "static_l2_hit",
                                "api static cache L2 hit"
                            );
                            self.store_l1_value(
                                key,
                                bytes.clone(),
                                L1Life::Static {
                                    ttl_secs,
                                    remaining_ms,
                                },
                            );
                            Ok(bytes)
                        } else {
                            tracing::warn!(
                                cache_status = "static_l2_invalid",
                                "api static cache L2 invalid"
                            );
                            self.fetch_and_maybe_cache_static_bytes(fetch, key, ttl_secs, options)
                                .await
                        }
                    }
                    Ok(L2ValueRead::Miss | L2ValueRead::NotFound) => {
                        incr(&CACHE_STATS.l2_miss);
                        tracing::debug!(cache_status = "static_l2_miss", "api static cache miss");
                        self.fetch_and_maybe_cache_static_bytes(fetch, key, ttl_secs, options)
                            .await
                    }
                    Err(err) => {
                        incr(&CACHE_STATS.l2_timeout);
                        tracing::warn!(%err, "api static cache read failed");
                        fetch.await
                    }
                }
            },
        )
        .await
    }

    async fn get_or_fetch_bytes_with_options<Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
        options: CacheOptions,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        if ttl_secs == 0 {
            return fetch.await;
        }
        let request = CacheRequest {
            server,
            event_id,
            suffix: &suffix,
            ttl_secs,
            options,
        };
        let control_key = control_cache_key(server, event_id);
        if let Some(control) = self.l1.get_control(&control_key) {
            return self
                .get_bytes_with_l1_control(request, control, fetch)
                .await;
        }
        self.get_bytes_without_l1_control(request, control_key, fetch)
            .await
    }

    async fn get_or_fetch_precompressed_bytes<Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        self.get_or_fetch_precompressed_bytes_with_options(
            server,
            event_id,
            suffix,
            ttl_secs,
            fetch,
            CacheOptions {
                max_value_bytes: self.cfg.max_value_bytes,
                is_batch: false,
                validate_cached_bytes: None,
            },
        )
        .await
    }

    async fn get_or_fetch_precompressed_bytes_with_options<Fut>(
        &self,
        server: &str,
        event_id: i64,
        suffix: String,
        ttl_secs: u64,
        fetch: Fut,
        options: CacheOptions,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        if ttl_secs == 0 {
            return self.encode_response(fetch.await?, None, options).await;
        }
        let request = CacheRequest {
            server,
            event_id,
            suffix: &suffix,
            ttl_secs,
            options,
        };
        let control_key = control_cache_key(server, event_id);
        if let Some(control) = self.l1.get_control(&control_key) {
            return self
                .get_encoded_with_l1_control(request, control, fetch)
                .await;
        }
        self.get_encoded_without_l1_control(request, control_key, fetch)
            .await
    }

    async fn get_bytes_with_l1_control<Fut>(
        &self,
        request: CacheRequest<'_>,
        control: L1Control,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        record_control_hit();
        if control.dirty {
            record_dirty_bypass();
            return self
                .fetch_bytes_with_singleflight(
                    dirty_flight_key(
                        request.server,
                        request.event_id,
                        control.epoch,
                        request.suffix,
                    ),
                    fetch,
                    None,
                    request.options,
                )
                .await;
        }

        let key = request.value_key(control.epoch);
        if let Some(bytes) = self.l1.get_value(&key) {
            record_l1_hit(request.options);
            tracing::debug!(
                cache_status = cache_status(request.options, "l1_hit"),
                "api cache L1 hit"
            );
            return Ok(bytes);
        }
        self.lookup_with_singleflight(
            lookup_flight_key(
                request.server,
                request.event_id,
                control.epoch,
                request.suffix,
            ),
            request.options,
            async {
                let read = self.read_l2_value(&key).await;
                self.resolve_l2_value(request, control.epoch, key, read, fetch)
                    .await
            },
        )
        .await
    }

    async fn resolve_l2_value<Fut>(
        &self,
        request: CacheRequest<'_>,
        epoch: i64,
        key: String,
        read: Result<L2ValueRead, redis::RedisError>,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        match read {
            Ok(L2ValueRead::Hit { bytes, .. })
                if cached_bytes_are_valid(request.options, &bytes) =>
            {
                record_l2_hit(request.options);
                tracing::debug!(
                    cache_status = cache_status(request.options, "l2_hit"),
                    "api cache L2 hit"
                );
                self.store_l1_value(key, bytes.clone(), request.epoch_life());
                Ok(bytes)
            }
            Ok(L2ValueRead::Hit { .. }) => {
                tracing::warn!(
                    cache_status = "l2_invalid",
                    "api cache L2 invalid, refetching"
                );
                self.fetch_and_maybe_cache_bytes(
                    fetch,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Ok(L2ValueRead::NotFound) => {
                record_l2_not_found();
                Err(ApiError::NotFound)
            }
            Ok(L2ValueRead::Miss) => {
                record_l2_miss(request.options);
                self.fetch_and_maybe_cache_bytes(
                    fetch,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Err(err) => {
                incr(&CACHE_STATS.l2_timeout);
                tracing::warn!(%err, "api cache value read failed");
                fetch.await
            }
        }
    }

    async fn get_bytes_without_l1_control<Fut>(
        &self,
        request: CacheRequest<'_>,
        control_key: String,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        self.lookup_with_singleflight(
            lookup_flight_key(request.server, request.event_id, -1, request.suffix),
            request.options,
            async {
                let read = self
                    .read_l2_combined(request.server, request.event_id, request.suffix)
                    .await;
                self.resolve_l2_combined_bytes(request, control_key, read, fetch)
                    .await
            },
        )
        .await
    }

    async fn resolve_l2_combined_bytes<Fut>(
        &self,
        request: CacheRequest<'_>,
        control_key: String,
        read: Result<L2CombinedRead, redis::RedisError>,
        fetch: Fut,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        match read {
            Ok(L2CombinedRead::Dirty { epoch }) => {
                self.store_l1_control(control_key, epoch, true);
                record_dirty_bypass();
                self.fetch_bytes_with_singleflight(
                    dirty_flight_key(request.server, request.event_id, epoch, request.suffix),
                    fetch,
                    None,
                    request.options,
                )
                .await
            }
            Ok(L2CombinedRead::Hit { epoch, key, bytes })
                if cached_bytes_are_valid(request.options, &bytes) =>
            {
                self.store_l1_control(control_key, epoch, false);
                self.store_l1_value(key, bytes.clone(), request.epoch_life());
                record_l2_hit(request.options);
                tracing::debug!(
                    cache_status = cache_status(request.options, "l2_hit"),
                    "api cache L2 hit"
                );
                Ok(bytes)
            }
            Ok(L2CombinedRead::Hit { epoch, key, .. }) => {
                self.store_l1_control(control_key, epoch, false);
                tracing::warn!(
                    cache_status = "l2_invalid",
                    "api cache L2 invalid, refetching"
                );
                self.fetch_and_maybe_cache_bytes(
                    fetch,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Ok(L2CombinedRead::NotFound { epoch }) => {
                self.store_l1_control(control_key, epoch, false);
                record_l2_not_found();
                Err(ApiError::NotFound)
            }
            Ok(L2CombinedRead::Miss { epoch, key }) => {
                self.store_l1_control(control_key, epoch, false);
                record_l2_miss(request.options);
                self.fetch_and_maybe_cache_bytes(
                    fetch,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Err(err) => {
                incr(&CACHE_STATS.l2_timeout);
                tracing::warn!(%err, "api cache combined read failed");
                fetch.await
            }
        }
    }

    async fn get_encoded_with_l1_control<Fut>(
        &self,
        request: CacheRequest<'_>,
        control: L1Control,
        fetch: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        record_control_hit();
        if control.dirty {
            record_dirty_bypass();
            return self
                .fetch_encoded_with_singleflight(
                    gzip_flight_key(
                        request.server,
                        request.event_id,
                        control.epoch,
                        request.suffix,
                    ),
                    fetch,
                    None,
                    request.options,
                )
                .await;
        }

        let key = request.value_key(control.epoch);
        let gzip = gzip_key(&key);
        if let Some(bytes) = self.l1.get_value(&gzip) {
            record_l1_hit(request.options);
            tracing::debug!(
                cache_status = cache_status(request.options, "l1_gzip_hit"),
                "api cache L1 gzip hit"
            );
            return Ok(CachedJson::gzip(bytes).at_epoch(Some(control.epoch)));
        }
        if let Some(bytes) = self.l1.get_value(&key) {
            record_l1_hit(request.options);
            tracing::debug!(
                cache_status = cache_status(request.options, "l1_hit"),
                "api cache L1 hit, building gzip"
            );
            return self
                .encode_response(
                    bytes,
                    Some(request.write_context(control.epoch, key)),
                    request.options,
                )
                .await;
        }
        self.lookup_encoded_with_singleflight(
            gzip_lookup_flight_key(
                request.server,
                request.event_id,
                control.epoch,
                request.suffix,
            ),
            request.options,
            async {
                let read = self.read_l2_encoded(&key, &gzip).await;
                self.resolve_l2_encoded(request, control.epoch, key, gzip, read, fetch)
                    .await
            },
        )
        .await
    }

    async fn resolve_l2_encoded<Fut>(
        &self,
        request: CacheRequest<'_>,
        epoch: i64,
        key: String,
        gzip: String,
        read: Result<L2EncodedRead, redis::RedisError>,
        fetch: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        match read {
            Ok(L2EncodedRead::Gzip(bytes)) => {
                record_l2_hit(request.options);
                tracing::debug!(
                    cache_status = cache_status(request.options, "l2_gzip_hit"),
                    "api cache L2 gzip hit"
                );
                self.store_l1_value(gzip, bytes.clone(), request.epoch_life());
                Ok(CachedJson::gzip(bytes).at_epoch(Some(epoch)))
            }
            Ok(L2EncodedRead::Identity(bytes)) => {
                record_l2_hit(request.options);
                tracing::debug!(
                    cache_status = cache_status(request.options, "l2_hit"),
                    "api cache L2 hit, building gzip"
                );
                self.store_l1_value(key.clone(), bytes.clone(), request.epoch_life());
                self.encode_response(
                    bytes,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Ok(L2EncodedRead::NotFound) => {
                record_l2_not_found();
                Err(ApiError::NotFound)
            }
            Ok(L2EncodedRead::Miss) => {
                record_l2_miss(request.options);
                self.fetch_and_maybe_cache_encoded(
                    fetch,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Err(err) => {
                incr(&CACHE_STATS.l2_timeout);
                tracing::warn!(%err, "api cache encoded read failed");
                self.encode_response(fetch.await?, None, request.options)
                    .await
            }
        }
    }

    async fn get_encoded_without_l1_control<Fut>(
        &self,
        request: CacheRequest<'_>,
        control_key: String,
        fetch: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        self.lookup_encoded_with_singleflight(
            gzip_lookup_flight_key(request.server, request.event_id, -1, request.suffix),
            request.options,
            async {
                let read = self
                    .read_l2_combined(request.server, request.event_id, request.suffix)
                    .await;
                self.resolve_l2_combined_encoded(request, control_key, read, fetch)
                    .await
            },
        )
        .await
    }

    async fn resolve_l2_combined_encoded<Fut>(
        &self,
        request: CacheRequest<'_>,
        control_key: String,
        read: Result<L2CombinedRead, redis::RedisError>,
        fetch: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        match read {
            Ok(L2CombinedRead::Dirty { epoch }) => {
                self.store_l1_control(control_key, epoch, true);
                record_dirty_bypass();
                self.fetch_encoded_with_singleflight(
                    gzip_flight_key(request.server, request.event_id, epoch, request.suffix),
                    fetch,
                    None,
                    request.options,
                )
                .await
            }
            Ok(L2CombinedRead::Hit { epoch, key, bytes }) => {
                self.store_l1_control(control_key, epoch, false);
                self.store_l1_value(key.clone(), bytes.clone(), request.epoch_life());
                record_l2_hit(request.options);
                tracing::debug!(
                    cache_status = cache_status(request.options, "l2_hit"),
                    "api cache L2 hit"
                );
                self.encode_response(
                    bytes,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Ok(L2CombinedRead::NotFound { epoch }) => {
                self.store_l1_control(control_key, epoch, false);
                record_l2_not_found();
                Err(ApiError::NotFound)
            }
            Ok(L2CombinedRead::Miss { epoch, key }) => {
                self.store_l1_control(control_key, epoch, false);
                record_l2_miss(request.options);
                self.fetch_and_maybe_cache_encoded(
                    fetch,
                    Some(request.write_context(epoch, key)),
                    request.options,
                )
                .await
            }
            Err(err) => {
                incr(&CACHE_STATS.l2_timeout);
                tracing::warn!(%err, "api cache combined read failed");
                self.encode_response(fetch.await?, None, request.options)
                    .await
            }
        }
    }

    async fn lookup_with_singleflight<Fut>(
        &self,
        flight_key: String,
        options: CacheOptions,
        lookup: Fut,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        match self.singleflight.begin(flight_key) {
            Flight::Waiter(entry) => {
                incr(&CACHE_STATS.lookup_singleflight_wait);
                if options.is_batch {
                    incr(&CACHE_STATS.batch_singleflight_wait);
                }
                tracing::debug!(
                    cache_status = cache_status(options, "lookup_singleflight_wait"),
                    "api cache waiting for in-flight lookup"
                );
                if let Some(value) = SingleFlight::wait_bytes(entry).await {
                    return value;
                }
                tracing::debug!(
                    cache_status = "lookup_singleflight_retry",
                    "api cache in-flight lookup was not shareable"
                );
                lookup.await
            }
            Flight::Owner(guard) => {
                let result = lookup.await;
                let shared = shared_fetch_bytes_result(&result);
                guard.finish(shared);
                result
            }
        }
    }

    async fn fetch_bytes_with_singleflight<Fut>(
        &self,
        flight_key: String,
        fetch: Fut,
        write_context: Option<CacheWriteContext>,
        options: CacheOptions,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        match self.singleflight.begin(flight_key) {
            Flight::Waiter(entry) => {
                incr(&CACHE_STATS.singleflight_wait);
                if options.is_batch {
                    incr(&CACHE_STATS.batch_singleflight_wait);
                }
                tracing::debug!(
                    cache_status = cache_status(options, "singleflight_wait"),
                    "api cache waiting for in-flight fetch"
                );
                if let Some(value) = SingleFlight::wait_bytes(entry).await {
                    return value;
                }
                tracing::debug!(
                    cache_status = "singleflight_retry",
                    "api cache in-flight fetch was not shareable"
                );
                fetch.await
            }
            Flight::Owner(guard) => {
                let result = self
                    .fetch_and_maybe_cache_bytes(fetch, write_context, options)
                    .await;
                let shared = shared_fetch_bytes_result(&result);
                guard.finish(shared);
                result
            }
        }
    }

    async fn lookup_encoded_with_singleflight<Fut>(
        &self,
        flight_key: String,
        options: CacheOptions,
        lookup: Fut,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<CachedJson, ApiError>>,
    {
        match self.singleflight.begin(flight_key) {
            Flight::Waiter(entry) => {
                incr(&CACHE_STATS.lookup_singleflight_wait);
                if options.is_batch {
                    incr(&CACHE_STATS.batch_singleflight_wait);
                }
                tracing::debug!(
                    cache_status = cache_status(options, "lookup_singleflight_wait"),
                    "api cache waiting for in-flight encoded lookup"
                );
                if let Some(value) = SingleFlight::wait_cached_json(entry).await {
                    return value;
                }
                tracing::debug!(
                    cache_status = "lookup_singleflight_retry",
                    "api cache in-flight encoded lookup was not shareable"
                );
                lookup.await
            }
            Flight::Owner(guard) => {
                let result = lookup.await;
                let shared = shared_cached_json_result(&result);
                guard.finish(shared);
                result
            }
        }
    }

    async fn fetch_encoded_with_singleflight<Fut>(
        &self,
        flight_key: String,
        fetch: Fut,
        write_context: Option<CacheWriteContext>,
        options: CacheOptions,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        match self.singleflight.begin(flight_key) {
            Flight::Waiter(entry) => {
                incr(&CACHE_STATS.singleflight_wait);
                if options.is_batch {
                    incr(&CACHE_STATS.batch_singleflight_wait);
                }
                tracing::debug!(
                    cache_status = cache_status(options, "singleflight_wait"),
                    "api cache waiting for in-flight encoded fetch"
                );
                if let Some(value) = SingleFlight::wait_cached_json(entry).await {
                    return value;
                }
                tracing::debug!(
                    cache_status = "singleflight_retry",
                    "api cache in-flight encoded fetch was not shareable"
                );
                self.encode_response(fetch.await?, None, options).await
            }
            Flight::Owner(guard) => {
                let result = self
                    .fetch_and_maybe_cache_encoded(fetch, write_context, options)
                    .await;
                let shared = shared_cached_json_result(&result);
                guard.finish(shared);
                result
            }
        }
    }

    async fn fetch_and_maybe_cache_bytes<Fut>(
        &self,
        fetch: Fut,
        write_context: Option<CacheWriteContext>,
        options: CacheOptions,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
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
                        Bytes::from_static(b"1"),
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

        let bytes = result?;
        if bytes.len() > options.max_value_bytes {
            if options.is_batch {
                incr(&CACHE_STATS.batch_too_large);
            }
            tracing::debug!(
                cache_status = if options.is_batch {
                    "batch_too_large"
                } else {
                    "too_large"
                },
                bytes = bytes.len(),
                max = options.max_value_bytes,
                "api cache value too large"
            );
            return Ok(bytes);
        }

        let Some(ctx) = write_context else {
            return Ok(bytes);
        };
        match self
            .write_l2_if_clean(&ctx, &ctx.value_key, bytes.clone(), ctx.ttl_secs)
            .await
        {
            Ok(true) => self.store_l1_value(ctx.value_key.clone(), bytes.clone(), ctx.life()),
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, "api cache write failed"),
        }
        Ok(bytes)
    }

    async fn fetch_and_maybe_cache_static_bytes<Fut>(
        &self,
        fetch: Fut,
        key: String,
        ttl_secs: u64,
        options: CacheOptions,
    ) -> Result<Bytes, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
    {
        let bytes = fetch.await?;
        if bytes.len() > options.max_value_bytes {
            tracing::debug!(
                cache_status = "static_too_large",
                bytes = bytes.len(),
                max = options.max_value_bytes,
                "api static cache value too large"
            );
            return Ok(bytes);
        }
        match self.write_l2_static(&key, bytes.clone(), ttl_secs).await {
            Ok(()) => self.store_l1_value(
                key,
                bytes.clone(),
                L1Life::Static {
                    ttl_secs,
                    remaining_ms: None,
                },
            ),
            Err(err) => tracing::warn!(%err, "api static cache write failed"),
        }
        Ok(bytes)
    }

    async fn fetch_and_maybe_cache_encoded<Fut>(
        &self,
        fetch: Fut,
        write_context: Option<CacheWriteContext>,
        options: CacheOptions,
    ) -> Result<CachedJson, ApiError>
    where
        Fut: Future<Output = Result<Bytes, ApiError>>,
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
                        Bytes::from_static(b"1"),
                        self.cfg.negative_ttl_secs,
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {}
                    Err(err) => tracing::warn!(%err, "api cache negative write failed"),
                }
            }
            return result.map(CachedJson::identity);
        }

        let bytes = result?;
        if bytes.len() > options.max_value_bytes {
            if options.is_batch {
                incr(&CACHE_STATS.batch_too_large);
            }
            tracing::debug!(
                cache_status = if options.is_batch {
                    "batch_too_large"
                } else {
                    "too_large"
                },
                bytes = bytes.len(),
                max = options.max_value_bytes,
                "api cache value too large"
            );
            return self.encode_response(bytes, None, options).await;
        }

        let Some(ctx) = write_context else {
            return self.encode_response(bytes, None, options).await;
        };
        let mut encoded = self
            .encode_response(bytes.clone(), Some(ctx.clone()), options)
            .await?;
        match self
            .write_l2_if_clean(&ctx, &ctx.value_key, bytes.clone(), ctx.ttl_secs)
            .await
        {
            Ok(true) => {
                self.store_l1_value(ctx.value_key.clone(), bytes, ctx.life());
                if encoded.encoding == CachedJsonEncoding::Gzip {
                    self.store_l1_value(
                        gzip_key(&ctx.value_key),
                        encoded.bytes.clone(),
                        ctx.life(),
                    );
                }
            }
            // A rejected write means the event went dirty or moved on while
            // this fetch ran, so the bytes can't be vouched for as `ctx.epoch`.
            Ok(false) => encoded.epoch = None,
            Err(err) => {
                encoded.epoch = None;
                tracing::warn!(%err, "api cache write failed");
            }
        }
        Ok(encoded)
    }

    async fn encode_response(
        &self,
        bytes: Bytes,
        write_context: Option<CacheWriteContext>,
        _options: CacheOptions,
    ) -> Result<CachedJson, ApiError> {
        let epoch = write_context.as_ref().map(|ctx| ctx.epoch);
        if bytes.len() < self.cfg.precompress_min_bytes {
            return Ok(CachedJson::identity(bytes).at_epoch(epoch));
        }
        let gzip = self.gzip_response_bytes(bytes).await?;
        if let Some(ctx) = write_context {
            match self
                .write_l2_if_clean(&ctx, &gzip_key(&ctx.value_key), gzip.clone(), ctx.ttl_secs)
                .await
            {
                Ok(true) => self.store_l1_value(gzip_key(&ctx.value_key), gzip.clone(), ctx.life()),
                Ok(false) => {}
                Err(err) => tracing::warn!(%err, "api cache gzip write failed"),
            }
        }
        Ok(CachedJson::gzip(gzip).at_epoch(epoch))
    }

    async fn gzip_response_bytes(&self, bytes: Bytes) -> Result<Bytes, ApiError> {
        let level = self.cfg.gzip_level;
        if bytes.len() < GZIP_SPAWN_BLOCKING_THRESHOLD {
            return gzip_bytes(&bytes, level);
        }
        tokio::task::spawn_blocking(move || gzip_bytes(&bytes, level))
            .await
            .map_err(|err| {
                tracing::warn!(%err, "gzip blocking task failed");
                ApiError::ServiceUnavailable("gzip encode error".into())
            })?
    }

    async fn read_l2_combined(
        &self,
        server: &str,
        event_id: i64,
        suffix: &str,
    ) -> Result<L2CombinedRead, redis::RedisError> {
        let mut conn = self.conns.connection();
        let base = base_key(server, event_id);
        let mut invocation = READ_SCRIPT_HANDLE.prepare_invoke();
        invocation
            .key(epoch_key(server, event_id))
            .key(dirty_key(server, event_id))
            .arg(base)
            .arg(suffix);
        let fut = invocation.invoke_async::<(i64, i64, Vec<u8>, i64, i64)>(&mut conn);
        let (epoch, dirty, value, negative, has_value) = self.with_timeout(fut).await?;
        if dirty != 0 {
            return Ok(L2CombinedRead::Dirty { epoch });
        }
        let key = value_key(server, event_id, epoch, suffix);
        if has_value != 0 {
            return Ok(L2CombinedRead::Hit {
                epoch,
                key,
                bytes: Bytes::from(value),
            });
        }
        if negative != 0 {
            return Ok(L2CombinedRead::NotFound { epoch });
        }
        Ok(L2CombinedRead::Miss { epoch, key })
    }

    /// Reads a value with its remaining L2 life (`PTTL`, same round trip),
    /// which bounds how long a static value may then serve from L1.
    async fn read_l2_value(&self, key: &str) -> Result<L2ValueRead, redis::RedisError> {
        let mut conn = self.conns.connection();
        let mut pipe = redis::pipe();
        pipe.get(key).pttl(key).exists(format!("{key}:not_found"));
        let fut = pipe.query_async::<(Option<Vec<u8>>, i64, bool)>(&mut conn);
        let (value, pttl_ms, negative) = self.with_timeout(fut).await?;
        if let Some(bytes) = value {
            Ok(L2ValueRead::Hit {
                bytes: Bytes::from(bytes),
                // -1 (no expiry) and -2 (gone) mean "unknown" here.
                remaining_ms: u64::try_from(pttl_ms).ok(),
            })
        } else if negative {
            Ok(L2ValueRead::NotFound)
        } else {
            Ok(L2ValueRead::Miss)
        }
    }

    async fn read_l2_encoded(
        &self,
        key: &str,
        gzip: &str,
    ) -> Result<L2EncodedRead, redis::RedisError> {
        let mut conn = self.conns.connection();
        let mut pipe = redis::pipe();
        pipe.get(gzip).get(key).exists(format!("{key}:not_found"));
        let fut = pipe.query_async::<(Option<Vec<u8>>, Option<Vec<u8>>, bool)>(&mut conn);
        let (gzip, value, negative) = self.with_timeout(fut).await?;
        if let Some(bytes) = gzip {
            Ok(L2EncodedRead::Gzip(Bytes::from(bytes)))
        } else if let Some(bytes) = value {
            Ok(L2EncodedRead::Identity(Bytes::from(bytes)))
        } else if negative {
            Ok(L2EncodedRead::NotFound)
        } else {
            Ok(L2EncodedRead::Miss)
        }
    }

    async fn write_l2_if_clean(
        &self,
        ctx: &CacheWriteContext,
        key: &str,
        bytes: Bytes,
        ttl_secs: u64,
    ) -> Result<bool, redis::RedisError> {
        let mut conn = self.conns.connection();
        let mut invocation = WRITE_SCRIPT_HANDLE.prepare_invoke();
        invocation
            .key(epoch_key(&ctx.server, ctx.event_id))
            .key(dirty_key(&ctx.server, ctx.event_id))
            .key(key)
            .arg(ctx.epoch)
            .arg(bytes.as_ref())
            .arg(ttl_secs);
        let fut = invocation.invoke_async::<i64>(&mut conn);
        self.with_timeout(fut).await.map(|written| written != 0)
    }

    async fn write_l2_static(
        &self,
        key: &str,
        bytes: Bytes,
        ttl_secs: u64,
    ) -> Result<(), redis::RedisError> {
        let mut conn = self.conns.connection();
        let fut = conn.set_ex::<_, _, ()>(key, bytes.as_ref(), ttl_secs);
        self.with_timeout(fut).await
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

    fn store_l1_control(&self, key: String, epoch: i64, dirty: bool) {
        if self.cfg.local_control_ttl_ms == 0 {
            return;
        }
        self.l1.insert_control(
            key,
            L1Control {
                epoch,
                dirty,
                expires_at: Instant::now() + Duration::from_millis(self.cfg.local_control_ttl_ms),
            },
        );
    }

    fn store_l1_value(&self, key: String, bytes: Bytes, life: L1Life) {
        let Some(ttl) = l1_value_ttl(&self.cfg, life) else {
            return;
        };
        self.l1.insert_value(
            key,
            L1Value {
                bytes,
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

/// How long a value may serve from L1, or `None` to keep it out.
///
/// Epoch keys keep the short local TTL: the control entry decides when an
/// epoch is stale, but the window queries behind those keys (snapshots,
/// overview, growth) end at the wall clock, so a value must never sit in
/// L1 past its L2 TTL or the window freezes. Static and time-bucketed keys
/// are immutable for their whole L2 life, so they may stay until L2 would
/// drop them, capped by `local_static_value_ttl_secs` (0 falls back to the
/// epoch rule).
fn l1_value_ttl(cfg: &ApiCacheConfig, life: L1Life) -> Option<Duration> {
    let local_ms = cfg.local_value_ttl_ms;
    if local_ms == 0 {
        return None;
    }
    let ms = match life {
        L1Life::Epoch { ttl_secs } => local_ms.min(ttl_secs.saturating_mul(1000)),
        L1Life::Static {
            ttl_secs,
            remaining_ms,
        } => {
            let l2_ms = remaining_ms.unwrap_or_else(|| ttl_secs.saturating_mul(1000));
            match cfg.local_static_value_ttl_secs.saturating_mul(1000) {
                0 => local_ms.min(l2_ms),
                cap_ms => cap_ms.min(l2_ms),
            }
        }
    };
    (ms > 0).then(|| Duration::from_millis(ms))
}

/// Which L1 lifetime rule a value falls under (see [`l1_value_ttl`]).
#[derive(Clone, Copy, Debug)]
enum L1Life {
    /// An epoch-keyed value whose L2 TTL is `ttl_secs`.
    Epoch { ttl_secs: u64 },
    /// A static-keyed value: `ttl_secs` is the L2 TTL it was written with,
    /// `remaining_ms` the L2 life left when it was read back (`None` when
    /// it was just written or Redis reported no expiry).
    Static {
        ttl_secs: u64,
        remaining_ms: Option<u64>,
    },
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

/// Independently locked L1 shards. quick_cache splits the byte budget
/// evenly across them, so more shards means a lower ceiling on the largest
/// (multi-MB batch) values; eight keeps that ceiling at budget / 8.
const L1_SHARDS: usize = 8;
/// Bookkeeping charged per value entry on top of key and payload bytes: the
/// key and `Bytes` handles, the deadline and the cache's own slot.
const L1_ENTRY_OVERHEAD: usize = 128;

/// In-process tier in front of Redis: control entries (epoch + dirty per
/// event) and value entries (response bytes), each with its own deadline.
///
/// Values are bounded by bytes, not count. Counting entries let a burst of
/// distinct minute-bucketed traces pin up to `max_entries` × 4 MB: those
/// keys are never read again after their minute, and expired entries used
/// to go only on a read or when a shard filled. Now the weighter charges
/// every entry its payload, the cache evicts to stay under the budget, and
/// a periodic sweep drops expired entries the reads never revisit.
#[derive(Clone)]
struct L1Cache {
    inner: Option<Arc<L1Inner>>,
}

type L1Controls = QuickCache<String, L1Control>;
type L1Values = QuickCache<String, L1Value, L1Weighter, DefaultHashBuilder, L1Lifecycle>;

struct L1Inner {
    controls: L1Controls,
    values: L1Values,
    max_bytes: usize,
}

#[derive(Clone, Copy)]
struct L1Control {
    epoch: i64,
    dirty: bool,
    expires_at: Instant,
}

#[derive(Clone)]
struct L1Value {
    bytes: Bytes,
    expires_at: Instant,
}

#[derive(Clone, Copy, Default)]
struct L1Weighter;

impl Weighter<String, L1Value> for L1Weighter {
    fn weight(&self, key: &String, value: &L1Value) -> u64 {
        (key.len() + value.bytes.len() + L1_ENTRY_OVERHEAD) as u64
    }
}

/// Counts budget evictions (and rejected oversized inserts, which the cache
/// reports the same way) so the stats log shows whether the budget binds.
#[derive(Clone, Copy, Default)]
struct L1Lifecycle;

impl Lifecycle<String, L1Value> for L1Lifecycle {
    type RequestState = ();

    fn begin_request(&self) {}

    fn on_evict(&self, _state: &mut (), _key: String, _value: L1Value) {
        incr(&CACHE_STATS.l1_evicted);
    }
}

impl L1Cache {
    /// `max_entries == 0` disables L1 entirely; `max_bytes == 0` keeps the
    /// (tiny) control entries but stores no values.
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        if max_entries == 0 {
            return Self { inner: None };
        }
        let options = |weight_capacity: u64| {
            OptionsBuilder::new()
                .shards(L1_SHARDS)
                .estimated_items_capacity(max_entries)
                .weight_capacity(weight_capacity)
                .build()
                .expect("L1 cache options are complete")
        };
        let controls = QuickCache::with_options(
            options(max_entries as u64),
            UnitWeighter,
            DefaultHashBuilder::default(),
            DefaultLifecycle::default(),
        );
        let values = QuickCache::with_options(
            options(max_bytes as u64),
            L1Weighter,
            DefaultHashBuilder::default(),
            L1Lifecycle,
        );
        Self {
            inner: Some(Arc::new(L1Inner {
                controls,
                values,
                max_bytes,
            })),
        }
    }

    fn get_control(&self, key: &str) -> Option<L1Control> {
        let inner = self.inner.as_ref()?;
        let control = inner.controls.get(key)?;
        if control.expires_at > Instant::now() {
            return Some(control);
        }
        inner.controls.remove(key);
        None
    }

    fn insert_control(&self, key: String, control: L1Control) {
        if let Some(inner) = &self.inner {
            inner.controls.insert(key, control);
        }
    }

    fn get_value(&self, key: &str) -> Option<Bytes> {
        let inner = self.inner.as_ref()?;
        let value = inner.values.get(key)?;
        if value.expires_at > Instant::now() {
            return Some(value.bytes);
        }
        inner.values.remove(key);
        None
    }

    fn insert_value(&self, key: String, value: L1Value) {
        if let Some(inner) = &self.inner
            && inner.max_bytes > 0
        {
            inner.values.insert(key, value);
        }
    }

    /// Drops every expired entry and refreshes the size gauges; returns how
    /// many entries went. The sweeper task calls the inner method directly.
    #[cfg(test)]
    fn sweep_expired(&self) -> usize {
        self.inner.as_ref().map_or(0, |inner| inner.sweep_expired())
    }

    /// Runs [`Self::sweep_expired`] every `interval` for as long as the
    /// cache is alive. Without a runtime (or with a zero interval) expired
    /// entries still go on read and under eviction pressure.
    fn spawn_sweeper(&self, interval: Duration) {
        let Some(inner) = &self.inner else {
            return;
        };
        if interval.is_zero() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("api cache L1 sweeper not started: no tokio runtime");
            return;
        };
        let weak = Arc::downgrade(inner);
        handle.spawn(async move {
            let mut ticker = time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                inner.sweep_expired();
            }
        });
    }
}

impl L1Inner {
    fn sweep_expired(&self) -> usize {
        let now = Instant::now();
        let expired = std::cell::Cell::new(0usize);
        let keep = |expires_at: Instant| {
            let live = expires_at > now;
            if !live {
                expired.set(expired.get() + 1);
            }
            live
        };
        self.controls.retain(|_, control| keep(control.expires_at));
        self.values.retain(|_, value| keep(value.expires_at));
        let expired = expired.get();
        add(&CACHE_STATS.l1_expired, expired as u64);
        set(&CACHE_STATS.l1_entries, self.values.len() as u64);
        set(&CACHE_STATS.l1_bytes, self.values.weight());
        expired
    }
}

fn lock_ignore_poison<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

enum L2CombinedRead {
    Dirty {
        epoch: i64,
    },
    Hit {
        epoch: i64,
        key: String,
        bytes: Bytes,
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
    Hit {
        bytes: Bytes,
        /// Remaining L2 life, when Redis reported one.
        remaining_ms: Option<u64>,
    },
    NotFound,
    Miss,
}

enum L2EncodedRead {
    Gzip(Bytes),
    Identity(Bytes),
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

#[derive(Clone, Copy)]
struct CacheRequest<'a> {
    server: &'a str,
    event_id: i64,
    suffix: &'a str,
    ttl_secs: u64,
    options: CacheOptions,
}

impl CacheWriteContext {
    fn life(&self) -> L1Life {
        L1Life::Epoch {
            ttl_secs: self.ttl_secs,
        }
    }
}

impl CacheRequest<'_> {
    fn value_key(self, epoch: i64) -> String {
        value_key(self.server, self.event_id, epoch, self.suffix)
    }

    fn epoch_life(self) -> L1Life {
        L1Life::Epoch {
            ttl_secs: self.ttl_secs,
        }
    }

    fn write_context(self, epoch: i64, value_key: String) -> CacheWriteContext {
        CacheWriteContext {
            server: self.server.to_owned(),
            event_id: self.event_id,
            epoch,
            negative_key: negative_key(self.server, self.event_id, epoch, self.suffix),
            value_key,
            ttl_secs: self.ttl_secs,
        }
    }
}

#[derive(Clone, Copy)]
struct CacheOptions {
    max_value_bytes: usize,
    is_batch: bool,
    validate_cached_bytes: Option<fn(&Bytes) -> bool>,
}

fn record_control_hit() {
    incr(&CACHE_STATS.l1_control_hit);
}

fn record_dirty_bypass() {
    incr(&CACHE_STATS.dirty_bypass);
    tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
}

fn record_l1_hit(options: CacheOptions) {
    incr(&CACHE_STATS.l1_hit);
    if options.is_batch {
        incr(&CACHE_STATS.batch_l1_hit);
    }
}

fn record_l2_hit(options: CacheOptions) {
    incr(&CACHE_STATS.l2_hit);
    if options.is_batch {
        incr(&CACHE_STATS.batch_l2_hit);
    }
}

fn record_l2_miss(options: CacheOptions) {
    incr(&CACHE_STATS.l2_miss);
    if options.is_batch {
        incr(&CACHE_STATS.batch_miss);
    }
    tracing::debug!(
        cache_status = cache_status(options, "l2_miss"),
        "api cache miss"
    );
}

fn record_l2_not_found() {
    incr(&CACHE_STATS.l2_not_found);
    tracing::debug!(cache_status = "l2_not_found", "api cache negative hit");
}

fn cache_status(options: CacheOptions, status: &'static str) -> &'static str {
    if !options.is_batch {
        return status;
    }
    match status {
        "l1_hit" | "l1_gzip_hit" => "batch_l1_hit",
        "l2_hit" | "l2_gzip_hit" => "batch_l2_hit",
        "l2_miss" => "batch_miss",
        "lookup_singleflight_wait" | "singleflight_wait" => "batch_singleflight_wait",
        _ => status,
    }
}

fn cached_bytes_are_valid(options: CacheOptions, bytes: &Bytes) -> bool {
    options
        .validate_cached_bytes
        .map(|validate| validate(bytes))
        .unwrap_or(true)
}

fn validate_json_bytes<T: DeserializeOwned>(bytes: &Bytes) -> bool {
    match sonic_rs::from_slice::<T>(bytes) {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, "api cache cached JSON schema mismatch");
            false
        }
    }
}

fn shared_fetch_bytes_result(result: &Result<Bytes, ApiError>) -> Option<SharedFetchResult> {
    match result {
        Ok(bytes) => Some(SharedFetchResult::Value(CachedJson::identity(
            bytes.clone(),
        ))),
        Err(ApiError::NotFound) => Some(SharedFetchResult::NotFound),
        Err(_) => None,
    }
}

fn shared_cached_json_result(result: &Result<CachedJson, ApiError>) -> Option<SharedFetchResult> {
    match result {
        Ok(value) => Some(SharedFetchResult::Value(value.clone())),
        Err(ApiError::NotFound) => Some(SharedFetchResult::NotFound),
        Err(_) => None,
    }
}

fn encode_json_bytes<T: Serialize>(value: &T) -> Result<Bytes, ApiError> {
    sonic_rs::to_vec(value).map(Bytes::from).map_err(|err| {
        tracing::error!(?err, "json encode error");
        ApiError::ServiceUnavailable("json encode error".into())
    })
}

fn gzip_bytes(bytes: &[u8], level: u32) -> Result<Bytes, ApiError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level.min(9)));
    encoder.write_all(bytes).map_err(|err| {
        tracing::warn!(%err, "gzip encode error");
        ApiError::ServiceUnavailable("gzip encode error".into())
    })?;
    encoder.finish().map(Bytes::from).map_err(|err| {
        tracing::warn!(%err, "gzip finish error");
        ApiError::ServiceUnavailable("gzip encode error".into())
    })
}

/// Both the flight map and per-entry state use `std` mutexes: no critical
/// section awaits, so the brief blocking lock beats an async mutex on a
/// path taken by every non-L1-hit request.
#[derive(Clone, Default)]
struct SingleFlight {
    inner: Arc<StdMutex<HashMap<String, Arc<InFlightEntry>>>>,
}

struct InFlightEntry {
    notify: Notify,
    state: StdMutex<InFlightState>,
}

#[derive(Default)]
struct InFlightState {
    done: bool,
    result: Option<SharedFetchResult>,
}

enum Flight {
    Owner(SingleFlightOwnerGuard),
    Waiter(Arc<InFlightEntry>),
}

enum SharedFetchResult {
    Value(CachedJson),
    NotFound,
}

struct SingleFlightOwnerGuard {
    singleflight: SingleFlight,
    key: String,
    entry: Option<Arc<InFlightEntry>>,
}

impl SingleFlightOwnerGuard {
    fn finish(mut self, result: Option<SharedFetchResult>) {
        if let Some(entry) = self.entry.take() {
            self.singleflight.finish(&self.key, &entry, result);
        }
    }
}

impl Drop for SingleFlightOwnerGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.singleflight.finish(&self.key, &entry, None);
        }
    }
}

impl SingleFlight {
    /// The owner guard takes ownership of `key`, so a lookup costs one key
    /// allocation total (the map's clone) instead of three.
    fn begin(&self, key: String) -> Flight {
        let entry = {
            let mut inner = lock_ignore_poison(&self.inner);
            if let Some(entry) = inner.get(&key) {
                return Flight::Waiter(entry.clone());
            }
            let entry = Arc::new(InFlightEntry {
                notify: Notify::new(),
                state: StdMutex::new(InFlightState::default()),
            });
            inner.insert(key.clone(), entry.clone());
            entry
        };
        Flight::Owner(SingleFlightOwnerGuard {
            singleflight: self.clone(),
            key,
            entry: Some(entry),
        })
    }

    fn finish(&self, key: &str, entry: &Arc<InFlightEntry>, result: Option<SharedFetchResult>) {
        {
            let mut state = lock_ignore_poison(&entry.state);
            state.done = true;
            state.result = result;
        }
        {
            let mut inner = lock_ignore_poison(&self.inner);
            if inner
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
            {
                inner.remove(key);
            }
        }
        entry.notify.notify_waiters();
    }

    async fn wait_bytes(entry: Arc<InFlightEntry>) -> Option<Result<Bytes, ApiError>> {
        loop {
            let notified = entry.notify.notified();
            {
                let state = lock_ignore_poison(&entry.state);
                if state.done {
                    return match &state.result {
                        Some(SharedFetchResult::Value(value)) => Some(Ok(value.bytes.clone())),
                        Some(SharedFetchResult::NotFound) => Some(Err(ApiError::NotFound)),
                        None => None,
                    };
                }
            }
            notified.await;
        }
    }

    async fn wait_cached_json(entry: Arc<InFlightEntry>) -> Option<Result<CachedJson, ApiError>> {
        loop {
            let notified = entry.notify.notified();
            {
                let state = lock_ignore_poison(&entry.state);
                if state.done {
                    return match &state.result {
                        Some(SharedFetchResult::Value(value)) => Some(Ok(value.clone())),
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

/// Bumps the event's cache epoch and clears its dirty flag; returns the new
/// epoch (the `version` realtime `updated` pushes carry).
pub async fn finish_event_update(
    conn: &mut ConnectionManager,
    server: impl std::fmt::Display,
    event_id: i64,
) -> Result<i64, redis::RedisError> {
    let server = server.to_string();
    let mut pipe = redis::pipe();
    pipe.incr(epoch_key(&server, event_id), 1)
        .del(dirty_key(&server, event_id))
        .ignore();
    let (epoch,) = pipe.query_async::<(i64,)>(conn).await?;
    Ok(epoch)
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
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:v{epoch}:{suffix}")
}

fn static_value_key(server: &str, event_id: i64, suffix: &str) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:static:{suffix}")
}

fn negative_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!("{}:not_found", value_key(server, event_id, epoch, suffix))
}

fn gzip_key(value_key: &str) -> String {
    format!("{value_key}:gz")
}

fn dirty_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:dirty:v{epoch}:{suffix}")
}

fn lookup_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:lookup:v{epoch}:{suffix}")
}

fn static_lookup_flight_key(server: &str, event_id: i64, suffix: &str) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:static_lookup:{suffix}")
}

fn gzip_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:gzip:v{epoch}:{suffix}")
}

fn gzip_lookup_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:gzip_lookup:v{epoch}:{suffix}")
}

fn control_cache_key(server: &str, event_id: i64) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:control")
}

fn epoch_key(server: &str, event_id: i64) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:epoch")
}

fn dirty_key(server: &str, event_id: i64) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache:dirty")
}

fn base_key(server: &str, event_id: i64) -> String {
    let server = lower_server(server);
    format!("haruki:tracker:{server}:{event_id}:api_cache")
}

/// Server strings are lowercase everywhere in practice (routes are parsed
/// through `SekaiServerRegion`); allocate only for the odd caller that
/// passes an uppercase form. Key bytes are unchanged either way.
fn lower_server(server: &str) -> std::borrow::Cow<'_, str> {
    if server.bytes().any(|b| b.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(server.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(server)
    }
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
    use serde::Deserialize;
    use std::sync::atomic::AtomicI64;

    static NEXT_EVENT_ID: AtomicI64 = AtomicI64::new(900_000);

    async fn coverage_redis() -> Option<ConnectionManager> {
        let Ok(url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
            return None;
        };
        let client = redis::Client::open(url).expect("coverage Redis URL should be valid");
        Some(
            ConnectionManager::new(client)
                .await
                .expect("coverage Redis should be reachable"),
        )
    }

    fn next_event_id() -> i64 {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        millis * 1_000 + NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed) % 1_000
    }

    fn test_config() -> ApiCacheConfig {
        ApiCacheConfig {
            enabled: true,
            local_control_ttl_ms: 60_000,
            local_value_ttl_ms: 60_000,
            precompress_min_bytes: 1,
            max_value_bytes: 1024 * 1024,
            batch_max_value_bytes: 4 * 1024 * 1024,
            ..ApiCacheConfig::default()
        }
    }

    fn test_cache(conn: &ConnectionManager, cfg: &ApiCacheConfig) -> ApiCache {
        ApiCache::new(vec![conn.clone()], cfg.clone())
    }

    #[tokio::test]
    async fn ttl_selection_and_zero_ttl_bypass_cover_public_entrypoints() {
        let Some(conn) = coverage_redis().await else {
            return;
        };
        let mut cfg = test_config();
        cfg.default_ttl_secs = 9;
        cfg.latest_rank_ttl_secs = 0;
        cfg.trace_rank_ttl_secs = 2;
        cfg.batch_trace_rank_ttl_secs = 3;
        cfg.user_data_ttl_secs = 4;
        cfg.replay_overview_ttl_secs = 5;
        let cache = test_cache(&conn, &cfg);
        assert_eq!(cache.ttl(CacheTtl::LatestRank), 9);
        assert_eq!(cache.ttl(CacheTtl::TraceRank), 2);
        assert_eq!(cache.ttl(CacheTtl::BatchTraceRank), 3);
        assert_eq!(cache.ttl(CacheTtl::UserData), 4);
        assert_eq!(cache.ttl(CacheTtl::ReplayOverview), 5);

        assert_eq!(
            cache
                .get_or_fetch("jp", 1, "typed".into(), 0, async { Ok(7_i64) })
                .await
                .unwrap(),
            7
        );
        assert_eq!(
            cache
                .get_or_fetch_static("jp", 1, "static".into(), 0, async { Ok(8_i64) })
                .await
                .unwrap(),
            8
        );
        assert_eq!(
            cache
                .get_or_fetch_json_bytes("jp", 1, "json".into(), 0, async { Ok(9_i64) })
                .await
                .unwrap(),
            Bytes::from_static(b"9")
        );
        assert_eq!(
            cache
                .get_or_fetch_static_json_bytes("jp", 1, "static-json".into(), 0, async {
                    Ok(10_i64)
                })
                .await
                .unwrap(),
            Bytes::from_static(b"10")
        );
        let encoded = cache
            .get_or_fetch_encoded_json("jp", 1, "encoded".into(), 0, false, async { Ok(11_i64) })
            .await
            .unwrap();
        assert_eq!(encoded.encoding, CachedJsonEncoding::Identity);
        let batch = cache
            .get_or_fetch_batch_encoded_json("jp", 1, "batch".into(), 0, false, async {
                Ok(Bytes::from_static(b"batch"))
            })
            .await
            .unwrap();
        assert_eq!(batch.bytes, Bytes::from_static(b"batch"));
    }

    #[tokio::test]
    async fn l2_resolvers_cover_hit_miss_negative_invalid_and_error_paths() {
        let Some(conn) = coverage_redis().await else {
            return;
        };
        let cache = test_cache(&conn, &test_config());
        let event_id = next_event_id();
        let options = CacheOptions {
            max_value_bytes: 1024,
            is_batch: false,
            validate_cached_bytes: None,
        };
        let request = CacheRequest {
            server: "jp",
            event_id,
            suffix: "resolver",
            ttl_secs: 60,
            options,
        };
        let key = request.value_key(0);
        let bytes = cache
            .resolve_l2_value(
                request,
                0,
                key.clone(),
                Ok(L2ValueRead::Hit {
                    bytes: Bytes::from_static(b"hit"),
                    remaining_ms: Some(1_000),
                }),
                async { Ok(Bytes::from_static(b"unused")) },
            )
            .await
            .unwrap();
        assert_eq!(bytes, Bytes::from_static(b"hit"));

        let invalid_request = CacheRequest {
            options: CacheOptions {
                validate_cached_bytes: Some(|_| false),
                ..options
            },
            ..request
        };
        let bytes = cache
            .resolve_l2_value(
                invalid_request,
                0,
                key.clone(),
                Ok(L2ValueRead::Hit {
                    bytes: Bytes::from_static(b"invalid"),
                    remaining_ms: None,
                }),
                async { Ok(Bytes::from_static(b"refetched")) },
            )
            .await
            .unwrap();
        assert_eq!(bytes, Bytes::from_static(b"refetched"));
        assert!(matches!(
            cache
                .resolve_l2_value(request, 0, key.clone(), Ok(L2ValueRead::NotFound), async {
                    Ok(Bytes::new())
                },)
                .await,
            Err(ApiError::NotFound)
        ));
        assert_eq!(
            cache
                .resolve_l2_value(request, 0, key.clone(), Ok(L2ValueRead::Miss), async {
                    Ok(Bytes::from_static(b"miss"))
                },)
                .await
                .unwrap(),
            Bytes::from_static(b"miss")
        );
        let redis_error = || redis::RedisError::from((redis::ErrorKind::Io, "test error"));
        assert_eq!(
            cache
                .resolve_l2_value(request, 0, key.clone(), Err(redis_error()), async {
                    Ok(Bytes::from_static(b"fallback"))
                },)
                .await
                .unwrap(),
            Bytes::from_static(b"fallback")
        );

        let gzip = gzip_key(&key);
        let encoded = cache
            .resolve_l2_encoded(
                request,
                0,
                key.clone(),
                gzip.clone(),
                Ok(L2EncodedRead::Gzip(Bytes::from_static(b"gzip"))),
                async { Ok(Bytes::new()) },
            )
            .await
            .unwrap();
        assert_eq!(encoded.encoding, CachedJsonEncoding::Gzip);
        let encoded = cache
            .resolve_l2_encoded(
                request,
                0,
                key.clone(),
                gzip.clone(),
                Ok(L2EncodedRead::Identity(Bytes::from_static(b"identity"))),
                async { Ok(Bytes::new()) },
            )
            .await
            .unwrap();
        assert_eq!(encoded.encoding, CachedJsonEncoding::Gzip);
        assert!(matches!(
            cache
                .resolve_l2_encoded(
                    request,
                    0,
                    key.clone(),
                    gzip.clone(),
                    Ok(L2EncodedRead::NotFound),
                    async { Ok(Bytes::new()) },
                )
                .await,
            Err(ApiError::NotFound)
        ));
        let encoded = cache
            .resolve_l2_encoded(
                request,
                0,
                key.clone(),
                gzip.clone(),
                Ok(L2EncodedRead::Miss),
                async { Ok(Bytes::from_static(b"miss")) },
            )
            .await
            .unwrap();
        assert_eq!(encoded.encoding, CachedJsonEncoding::Gzip);
        let encoded = cache
            .resolve_l2_encoded(request, 0, key, gzip, Err(redis_error()), async {
                Ok(Bytes::from_static(b"fallback"))
            })
            .await
            .unwrap();
        assert_eq!(encoded.encoding, CachedJsonEncoding::Gzip);
    }

    async fn redis_set(conn: &mut ConnectionManager, key: &str, value: &[u8]) {
        redis::cmd("SET")
            .arg(key)
            .arg(value)
            .query_async::<()>(conn)
            .await
            .unwrap();
    }

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
            lookup_flight_key("JP", 137, 3, "trace:rank:100"),
            "haruki:tracker:jp:137:api_cache:lookup:v3:trace:rank:100"
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

    fn l1_value(bytes: &'static [u8], ttl: Duration) -> L1Value {
        L1Value {
            bytes: Bytes::from_static(bytes),
            expires_at: Instant::now() + ttl,
        }
    }

    fn expired_l1_value(bytes: &'static [u8]) -> L1Value {
        L1Value {
            bytes: Bytes::from_static(bytes),
            expires_at: Instant::now() - Duration::from_secs(1),
        }
    }

    fn l1_control(epoch: i64, dirty: bool, ttl: Duration) -> L1Control {
        L1Control {
            epoch,
            dirty,
            expires_at: Instant::now() + ttl,
        }
    }

    fn l1_resident(l1: &L1Cache) -> (usize, u64) {
        let inner = l1.inner.as_ref().expect("L1 enabled");
        (inner.values.len(), inner.values.weight())
    }

    #[test]
    fn l1_value_hit_returns_cached_bytes() {
        let l1 = L1Cache::new(16, 1 << 20);
        l1.insert_value(
            "value".to_owned(),
            l1_value(b"cached", Duration::from_secs(1)),
        );

        assert_eq!(l1.get_value("value"), Some(Bytes::from_static(b"cached")));
    }

    #[test]
    fn l1_value_expiry_removes_cached_bytes() {
        let l1 = L1Cache::new(16, 1 << 20);
        l1.insert_value("value".to_owned(), expired_l1_value(b"stale"));

        assert_eq!(l1.get_value("value"), None);
        assert_eq!(l1.get_value("value"), None);
        assert_eq!(l1_resident(&l1).0, 0);
    }

    #[test]
    fn l1_control_tracks_epoch_and_dirty_state() {
        let l1 = L1Cache::new(16, 1 << 20);
        l1.insert_control(
            "control".to_owned(),
            l1_control(7, true, Duration::from_secs(1)),
        );

        let control = l1.get_control("control").unwrap();
        assert_eq!(control.epoch, 7);
        assert!(control.dirty);

        l1.insert_control(
            "control".to_owned(),
            L1Control {
                epoch: 8,
                dirty: false,
                expires_at: Instant::now() - Duration::from_millis(1),
            },
        );
        assert!(l1.get_control("control").is_none());
    }

    #[test]
    fn l1_zero_max_entries_disables_storage() {
        let l1 = L1Cache::new(0, 1 << 20);
        l1.insert_value(
            "value".to_owned(),
            l1_value(b"cached", Duration::from_secs(1)),
        );
        l1.insert_control(
            "control".to_owned(),
            l1_control(1, false, Duration::from_secs(1)),
        );

        assert_eq!(l1.get_value("value"), None);
        assert!(l1.get_control("control").is_none());
        assert_eq!(l1.sweep_expired(), 0);
    }

    #[test]
    fn l1_zero_max_bytes_keeps_controls_but_no_values() {
        let l1 = L1Cache::new(16, 0);
        l1.insert_value(
            "value".to_owned(),
            l1_value(b"cached", Duration::from_secs(1)),
        );
        l1.insert_control(
            "control".to_owned(),
            l1_control(3, false, Duration::from_secs(1)),
        );

        assert_eq!(l1.get_value("value"), None);
        assert_eq!(l1.get_control("control").unwrap().epoch, 3);
    }

    #[test]
    fn l1_byte_budget_bounds_resident_bytes() {
        const BUDGET: usize = 64 * 1024;
        let l1 = L1Cache::new(4096, BUDGET);
        let evicted_before = CACHE_STATS.l1_evicted.load(Ordering::Relaxed);
        for i in 0..256 {
            l1.insert_value(
                format!("trace:rank:{i}:b1"),
                L1Value {
                    bytes: Bytes::from(vec![b'x'; 4096]),
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            );
        }

        let (entries, bytes) = l1_resident(&l1);
        assert!(bytes <= BUDGET as u64, "{bytes} resident of {BUDGET}");
        assert!((1..256).contains(&entries), "{entries} entries");
        assert!(CACHE_STATS.l1_evicted.load(Ordering::Relaxed) > evicted_before);
        // The survivors are still served.
        let served = (0..256)
            .filter(|i| l1.get_value(&format!("trace:rank:{i}:b1")).is_some())
            .count();
        assert_eq!(served, entries);
    }

    #[test]
    fn l1_value_larger_than_a_shard_is_not_admitted() {
        let l1 = L1Cache::new(4096, 64 * 1024);
        l1.insert_value(
            "huge".to_owned(),
            L1Value {
                bytes: Bytes::from(vec![b'x'; 65 * 1024]),
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        assert_eq!(l1.get_value("huge"), None);
        assert_eq!(l1_resident(&l1), (0, 0));
    }

    #[test]
    fn l1_sweep_drops_expired_entries_and_refreshes_gauges() {
        let l1 = L1Cache::new(64, 1 << 20);
        l1.insert_value("expired".to_owned(), expired_l1_value(b"stale"));
        l1.insert_value(
            "live".to_owned(),
            l1_value(b"fresh", Duration::from_secs(60)),
        );
        l1.insert_control(
            "expired-control".to_owned(),
            L1Control {
                epoch: 1,
                dirty: false,
                expires_at: Instant::now() - Duration::from_millis(1),
            },
        );
        l1.insert_control(
            "live-control".to_owned(),
            l1_control(2, false, Duration::from_secs(60)),
        );
        let expired_before = CACHE_STATS.l1_expired.load(Ordering::Relaxed);

        assert_eq!(l1.sweep_expired(), 2);

        assert_eq!(l1.get_value("expired"), None);
        assert_eq!(l1.get_value("live"), Some(Bytes::from_static(b"fresh")));
        assert!(l1.get_control("expired-control").is_none());
        assert_eq!(l1.get_control("live-control").unwrap().epoch, 2);
        let (entries, bytes) = l1_resident(&l1);
        assert_eq!(entries, 1);
        assert_eq!(
            bytes,
            ("live".len() + "fresh".len() + L1_ENTRY_OVERHEAD) as u64
        );
        assert!(CACHE_STATS.l1_expired.load(Ordering::Relaxed) >= expired_before + 2);
        // Gauges are global, so another test may have refreshed them since;
        // an empty cache still reports its own state after a sweep.
        assert_eq!(l1.sweep_expired(), 0);
    }

    #[tokio::test]
    async fn l1_sweeper_task_expires_entries_without_reads() {
        let l1 = L1Cache::new(64, 1 << 20);
        l1.insert_value("soon".to_owned(), l1_value(b"x", Duration::from_millis(20)));
        l1.spawn_sweeper(Duration::from_millis(10));

        tokio::time::sleep(Duration::from_millis(80)).await;
        let (entries, _) = l1_resident(&l1);
        assert_eq!(entries, 0);

        // The task ends with the cache.
        let weak = Arc::downgrade(l1.inner.as_ref().unwrap());
        drop(l1);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn l1_value_ttl_never_exceeds_the_l2_ttl() {
        let cfg = ApiCacheConfig {
            local_value_ttl_ms: 250,
            local_static_value_ttl_secs: 60,
            ..ApiCacheConfig::default()
        };
        let ms = |life| l1_value_ttl(&cfg, life).map(|ttl| ttl.as_millis() as u64);

        // Epoch keys: the short local TTL, never past L2.
        assert_eq!(ms(L1Life::Epoch { ttl_secs: 60 }), Some(250));
        assert_eq!(ms(L1Life::Epoch { ttl_secs: 1 }), Some(250));
        let short = ApiCacheConfig {
            local_value_ttl_ms: 5_000,
            ..cfg.clone()
        };
        assert_eq!(
            l1_value_ttl(&short, L1Life::Epoch { ttl_secs: 1 }),
            Some(Duration::from_secs(1))
        );

        // Static keys: until L2 drops them, capped by the static ceiling.
        assert_eq!(
            ms(L1Life::Static {
                ttl_secs: 60,
                remaining_ms: None
            }),
            Some(60_000)
        );
        assert_eq!(
            ms(L1Life::Static {
                ttl_secs: 60,
                remaining_ms: Some(300)
            }),
            Some(300)
        );
        assert_eq!(
            ms(L1Life::Static {
                ttl_secs: 3600,
                remaining_ms: Some(3_000_000)
            }),
            Some(60_000)
        );
        assert_eq!(
            ms(L1Life::Static {
                ttl_secs: 60,
                remaining_ms: Some(0)
            }),
            None
        );

        // A zero static ceiling means the epoch rule.
        let legacy = ApiCacheConfig {
            local_static_value_ttl_secs: 0,
            ..cfg.clone()
        };
        assert_eq!(
            l1_value_ttl(
                &legacy,
                L1Life::Static {
                    ttl_secs: 60,
                    remaining_ms: None
                }
            ),
            Some(Duration::from_millis(250))
        );

        // No local value TTL disables L1 values outright.
        let off = ApiCacheConfig {
            local_value_ttl_ms: 0,
            ..cfg
        };
        assert_eq!(l1_value_ttl(&off, L1Life::Epoch { ttl_secs: 60 }), None);
        assert_eq!(
            l1_value_ttl(
                &off,
                L1Life::Static {
                    ttl_secs: 60,
                    remaining_ms: None
                }
            ),
            None
        );
    }

    #[test]
    fn l1_concurrent_inserts_reads_and_sweeps_stay_within_budget() {
        const BUDGET: usize = 256 * 1024;
        let l1 = L1Cache::new(4096, BUDGET);
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let l1 = l1.clone();
                std::thread::spawn(move || {
                    for i in 0..2_000u32 {
                        let key = format!("k:{}:{}", t, i % 128);
                        let len = 16 + (i as usize * 613) % 8_192;
                        let ttl = if i % 7 == 0 {
                            Duration::ZERO
                        } else {
                            Duration::from_secs(60)
                        };
                        l1.insert_value(
                            key.clone(),
                            L1Value {
                                bytes: Bytes::from(vec![b'x'; len]),
                                expires_at: Instant::now() + ttl,
                            },
                        );
                        if let Some(bytes) = l1.get_value(&key) {
                            assert_eq!(bytes.len(), len);
                        }
                        l1.get_value(&format!("k:{}:{}", (t + 1) % 8, i % 128));
                        if i % 250 == 0 {
                            l1.sweep_expired();
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        l1.sweep_expired();
        let (entries, bytes) = l1_resident(&l1);
        assert!(bytes <= BUDGET as u64, "{bytes} resident of {BUDGET}");
        assert!(entries > 0);
    }

    #[derive(Deserialize, Serialize, PartialEq, Debug)]
    struct TypedCachePayload {
        #[serde(default)]
        items: Vec<i64>,
    }

    #[test]
    fn typed_cache_validation_allows_default_empty_collections() {
        assert!(validate_json_bytes::<TypedCachePayload>(
            &Bytes::from_static(br#"{}"#)
        ));
        assert!(validate_json_bytes::<TypedCachePayload>(
            &Bytes::from_static(br#"{"items":[2]}"#)
        ));
    }

    #[derive(Deserialize, Serialize, PartialEq, Debug)]
    struct StrictTypedCachePayload {
        items: Vec<i64>,
    }

    #[test]
    fn typed_cache_validation_rejects_schema_mismatch() {
        assert!(!validate_json_bytes::<StrictTypedCachePayload>(
            &Bytes::from_static(br#"{"oldItems":[1]}"#)
        ));
    }

    #[tokio::test]
    async fn singleflight_shares_success_bytes() {
        let singleflight = SingleFlight::default();
        let key = "trace:rank:1".to_owned();
        let owner = match singleflight.begin(key.clone()) {
            Flight::Owner(guard) => guard,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()) {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };

        let task =
            tokio::spawn(async move { SingleFlight::wait_bytes(waiter).await.unwrap().unwrap() });
        owner.finish(Some(SharedFetchResult::Value(CachedJson::identity(
            Bytes::from_static(b"{\"ok\":true}"),
        ))));

        assert_eq!(task.await.unwrap(), Bytes::from_static(b"{\"ok\":true}"));
    }

    #[tokio::test]
    async fn singleflight_shares_not_found_result() {
        let singleflight = SingleFlight::default();
        let key = "trace:rank:40000".to_owned();
        let owner = match singleflight.begin(key.clone()) {
            Flight::Owner(guard) => guard,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()) {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };

        let task = tokio::spawn(async move { SingleFlight::wait_bytes(waiter).await.unwrap() });
        owner.finish(Some(SharedFetchResult::NotFound));

        assert!(matches!(task.await.unwrap(), Err(ApiError::NotFound)));
    }

    #[test]
    fn gzip_bytes_roundtrips_json() {
        let source = Bytes::from_static(br#"{"ok":true,"items":[1,2,3]}"#);
        let encoded = gzip_bytes(&source, 1).unwrap();
        assert!(encoded.len() > 10);

        let mut decoder = flate2::read::GzDecoder::new(encoded.as_ref());
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decoded).unwrap();
        assert_eq!(decoded, source.as_ref());
    }

    #[tokio::test]
    async fn singleflight_shares_gzip_cached_json() {
        let singleflight = SingleFlight::default();
        let key = "trace:rank:1:gzip".to_owned();
        let owner = match singleflight.begin(key.clone()) {
            Flight::Owner(guard) => guard,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()) {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };

        let task = tokio::spawn(async move {
            SingleFlight::wait_cached_json(waiter)
                .await
                .unwrap()
                .unwrap()
        });
        owner.finish(Some(SharedFetchResult::Value(CachedJson::gzip(
            Bytes::from_static(b"gzipped"),
        ))));

        let shared = task.await.unwrap();
        assert_eq!(shared.encoding, CachedJsonEncoding::Gzip);
        assert_eq!(shared.bytes, Bytes::from_static(b"gzipped"));
    }

    #[tokio::test]
    async fn singleflight_owner_drop_releases_waiters_and_key() {
        let singleflight = SingleFlight::default();
        let key = "trace:ranks:1,2,3".to_owned();
        let guard = match singleflight.begin(key.clone()) {
            Flight::Owner(guard) => guard,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()) {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };
        let task = tokio::spawn(async move { SingleFlight::wait_bytes(waiter).await });

        drop(guard);

        assert!(task.await.unwrap().is_none());
        match singleflight.begin(key) {
            Flight::Owner(_) => {}
            Flight::Waiter(_) => panic!("dropped owner should remove in-flight key"),
        }
    }

    #[tokio::test]
    async fn redis_cache_covers_dynamic_l1_l2_dirty_and_negative_paths() {
        let Some(mut conn) = coverage_redis().await else {
            return;
        };
        let event_id = next_event_id();
        let cfg = test_config();
        let cache = test_cache(&conn, &cfg);

        let first = cache
            .get_or_fetch::<TypedCachePayload, _>("JP", event_id, "typed".into(), 60, async {
                Ok(TypedCachePayload { items: vec![1] })
            })
            .await
            .unwrap();
        assert_eq!(first.items, vec![1]);

        let l1 = cache
            .get_or_fetch::<TypedCachePayload, _>("jp", event_id, "typed".into(), 60, async {
                Err(ApiError::ServiceUnavailable("should not fetch".into()))
            })
            .await
            .unwrap();
        assert_eq!(l1.items, vec![1]);

        let l2_cache = test_cache(&conn, &cfg);
        let l2 = l2_cache
            .get_or_fetch_json_bytes::<TypedCachePayload, _>(
                "jp",
                event_id,
                "typed".into(),
                60,
                async { Err(ApiError::ServiceUnavailable("should not fetch".into())) },
            )
            .await
            .unwrap();
        assert_eq!(
            sonic_rs::from_slice::<TypedCachePayload>(&l2)
                .unwrap()
                .items,
            vec![1]
        );

        let control_miss = l2_cache
            .get_or_fetch_json_bytes::<TypedCachePayload, _>(
                "jp",
                event_id,
                "other".into(),
                60,
                async { Ok(TypedCachePayload { items: vec![2] }) },
            )
            .await
            .unwrap();
        assert_eq!(
            sonic_rs::from_slice::<TypedCachePayload>(&control_miss)
                .unwrap()
                .items,
            vec![2]
        );

        begin_event_update(&mut conn, "JP", event_id).await.unwrap();
        let l1_dirty_cache = test_cache(&conn, &cfg);
        l1_dirty_cache.store_l1_control(control_cache_key("jp", event_id), 0, true);
        let l1_dirty = l1_dirty_cache
            .get_or_fetch_json_bytes::<TypedCachePayload, _>(
                "jp",
                event_id,
                "dirty-l1".into(),
                60,
                async { Ok(TypedCachePayload { items: vec![30] }) },
            )
            .await
            .unwrap();
        assert_eq!(
            sonic_rs::from_slice::<TypedCachePayload>(&l1_dirty)
                .unwrap()
                .items,
            vec![30]
        );

        let dirty_cache = test_cache(&conn, &cfg);
        let dirty = dirty_cache
            .get_or_fetch_json_bytes::<TypedCachePayload, _>(
                "jp",
                event_id,
                "dirty".into(),
                60,
                async { Ok(TypedCachePayload { items: vec![3] }) },
            )
            .await
            .unwrap();
        assert_eq!(
            sonic_rs::from_slice::<TypedCachePayload>(&dirty)
                .unwrap()
                .items,
            vec![3]
        );
        abort_event_update(&mut conn, "jp", event_id).await.unwrap();

        let negative_cache = test_cache(&conn, &cfg);
        assert!(matches!(
            negative_cache
                .get_or_fetch_json_bytes::<TypedCachePayload, _>(
                    "jp",
                    event_id,
                    "missing".into(),
                    60,
                    async { Err(ApiError::NotFound) },
                )
                .await,
            Err(ApiError::NotFound)
        ));
        let negative_l2 = test_cache(&conn, &cfg);
        assert!(matches!(
            negative_l2
                .get_or_fetch_json_bytes::<TypedCachePayload, _>(
                    "jp",
                    event_id,
                    "missing".into(),
                    60,
                    async { Ok(TypedCachePayload { items: vec![99] }) },
                )
                .await,
            Err(ApiError::NotFound)
        ));

        finish_event_update(&mut conn, "jp", event_id)
            .await
            .unwrap();
        let epoch_cache = test_cache(&conn, &cfg);
        let refreshed = epoch_cache
            .get_or_fetch_json_bytes::<TypedCachePayload, _>(
                "jp",
                event_id,
                "typed".into(),
                60,
                async { Ok(TypedCachePayload { items: vec![4] }) },
            )
            .await
            .unwrap();
        assert_eq!(
            sonic_rs::from_slice::<TypedCachePayload>(&refreshed)
                .unwrap()
                .items,
            vec![4]
        );
    }

    #[tokio::test]
    async fn redis_cache_refetches_invalid_dynamic_and_static_json() {
        let Some(mut conn) = coverage_redis().await else {
            return;
        };
        let event_id = next_event_id();
        let cfg = test_config();
        let dynamic_key = value_key("jp", event_id, 0, "invalid");
        redis_set(&mut conn, &dynamic_key, br#"{"oldItems":[1]}"#).await;

        let cache = test_cache(&conn, &cfg);
        let dynamic = cache
            .get_or_fetch::<StrictTypedCachePayload, _>(
                "jp",
                event_id,
                "invalid".into(),
                60,
                async { Ok(StrictTypedCachePayload { items: vec![5] }) },
            )
            .await
            .unwrap();
        assert_eq!(dynamic.items, vec![5]);

        let controlled_key = value_key("jp", event_id, 0, "controlled-invalid");
        redis_set(&mut conn, &controlled_key, br#"{"oldItems":[2]}"#).await;
        let controlled = test_cache(&conn, &cfg);
        controlled.store_l1_control(control_cache_key("jp", event_id), 0, false);
        let result = controlled
            .get_or_fetch::<StrictTypedCachePayload, _>(
                "jp",
                event_id,
                "controlled-invalid".into(),
                60,
                async { Ok(StrictTypedCachePayload { items: vec![6] }) },
            )
            .await
            .unwrap();
        assert_eq!(result.items, vec![6]);

        let static_key = static_value_key("jp", event_id, "static-invalid");
        redis_set(&mut conn, &static_key, br#"{"oldItems":[3]}"#).await;
        let static_cache = test_cache(&conn, &cfg);
        let result = static_cache
            .get_or_fetch_static::<StrictTypedCachePayload, _>(
                "jp",
                event_id,
                "static-invalid".into(),
                60,
                async { Ok(StrictTypedCachePayload { items: vec![7] }) },
            )
            .await
            .unwrap();
        assert_eq!(result.items, vec![7]);

        let static_l2 = test_cache(&conn, &cfg)
            .get_or_fetch_static_json_bytes::<StrictTypedCachePayload, _>(
                "jp",
                event_id,
                "static-invalid".into(),
                60,
                async { Err(ApiError::ServiceUnavailable("should not fetch".into())) },
            )
            .await
            .unwrap();
        assert_eq!(
            sonic_rs::from_slice::<StrictTypedCachePayload>(&static_l2)
                .unwrap()
                .items,
            vec![7]
        );
    }

    #[tokio::test]
    async fn redis_cache_covers_gzip_l1_l2_and_batch_paths() {
        let Some(mut conn) = coverage_redis().await else {
            return;
        };
        let event_id = next_event_id();
        let cfg = test_config();
        let cache = test_cache(&conn, &cfg);

        let encoded = cache
            .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                "jp",
                event_id,
                "gzip".into(),
                60,
                true,
                async {
                    Ok(TypedCachePayload {
                        items: (0..128).collect(),
                    })
                },
            )
            .await
            .unwrap();
        assert_eq!(encoded.encoding, CachedJsonEncoding::Gzip);

        let l1_gzip = cache
            .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                "jp",
                event_id,
                "gzip".into(),
                60,
                true,
                async { Err(ApiError::ServiceUnavailable("should not fetch".into())) },
            )
            .await
            .unwrap();
        assert_eq!(l1_gzip.encoding, CachedJsonEncoding::Gzip);

        let combined_l2 = test_cache(&conn, &cfg)
            .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                "jp",
                event_id,
                "gzip".into(),
                60,
                true,
                async { Err(ApiError::ServiceUnavailable("should not fetch".into())) },
            )
            .await
            .unwrap();
        assert_eq!(combined_l2.encoding, CachedJsonEncoding::Gzip);

        let l2_cache = test_cache(&conn, &cfg);
        l2_cache.store_l1_control(control_cache_key("jp", event_id), 0, false);
        let l2_gzip = l2_cache
            .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                "jp",
                event_id,
                "gzip".into(),
                60,
                true,
                async { Err(ApiError::ServiceUnavailable("should not fetch".into())) },
            )
            .await
            .unwrap();
        assert_eq!(l2_gzip.encoding, CachedJsonEncoding::Gzip);

        let value = value_key("jp", event_id, 0, "identity-only");
        redis_set(&mut conn, &value, br#"{"items":[8,9]}"#).await;
        let identity_cache = test_cache(&conn, &cfg);
        identity_cache.store_l1_control(control_cache_key("jp", event_id), 0, false);
        let rebuilt = identity_cache
            .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                "jp",
                event_id,
                "identity-only".into(),
                60,
                true,
                async { Err(ApiError::ServiceUnavailable("should not fetch".into())) },
            )
            .await
            .unwrap();
        assert_eq!(rebuilt.encoding, CachedJsonEncoding::Gzip);

        let batch = cache
            .get_or_fetch_batch_encoded_json("jp", event_id, "batch".into(), 60, true, async {
                Ok(Bytes::from_static(br#"{"items":[10]}"#))
            })
            .await
            .unwrap();
        assert_eq!(batch.encoding, CachedJsonEncoding::Gzip);

        let identity = cache
            .get_or_fetch_batch_encoded_json(
                "jp",
                event_id,
                "batch-identity".into(),
                60,
                false,
                async { Ok(Bytes::from_static(br#"{"items":[11]}"#)) },
            )
            .await
            .unwrap();
        assert_eq!(identity.encoding, CachedJsonEncoding::Identity);

        begin_event_update(&mut conn, "jp", event_id).await.unwrap();
        let dirty_cache = test_cache(&conn, &cfg);
        dirty_cache.store_l1_control(control_cache_key("jp", event_id), 0, true);
        let dirty = dirty_cache
            .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                "jp",
                event_id,
                "dirty-gzip".into(),
                60,
                true,
                async { Ok(TypedCachePayload { items: vec![14] }) },
            )
            .await
            .unwrap();
        assert_eq!(dirty.encoding, CachedJsonEncoding::Gzip);
        abort_event_update(&mut conn, "jp", event_id).await.unwrap();
    }

    #[tokio::test]
    async fn redis_cache_covers_encoded_negative_miss_and_identity_fallbacks() {
        let Some(conn) = coverage_redis().await else {
            return;
        };
        let event_id = next_event_id();
        let mut cfg = test_config();
        let cache = test_cache(&conn, &cfg);

        assert!(matches!(
            cache
                .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                    "jp",
                    event_id,
                    "encoded-missing".into(),
                    60,
                    true,
                    async { Err(ApiError::NotFound) },
                )
                .await,
            Err(ApiError::NotFound)
        ));
        let negative = test_cache(&conn, &cfg);
        negative.store_l1_control(control_cache_key("jp", event_id), 0, false);
        assert!(matches!(
            negative
                .get_or_fetch_encoded_json::<TypedCachePayload, _>(
                    "jp",
                    event_id,
                    "encoded-missing".into(),
                    60,
                    true,
                    async { Ok(TypedCachePayload { items: vec![1] }) },
                )
                .await,
            Err(ApiError::NotFound)
        ));

        cfg.precompress_gzip_enabled = false;
        let identity_cache = test_cache(&conn, &cfg);
        let identity = identity_cache
            .get_or_fetch_encoded_json("jp", event_id, "disabled-gzip".into(), 60, true, async {
                Ok(TypedCachePayload { items: vec![12] })
            })
            .await
            .unwrap();
        assert_eq!(identity.encoding, CachedJsonEncoding::Identity);

        let bypass = identity_cache
            .get_or_fetch_static_json_bytes("jp", event_id, "ttl-zero".into(), 0, async {
                Ok(TypedCachePayload { items: vec![13] })
            })
            .await
            .unwrap();
        assert_eq!(
            sonic_rs::from_slice::<TypedCachePayload>(&bypass)
                .unwrap()
                .items,
            vec![13]
        );
    }

    #[tokio::test]
    async fn cache_handles_oversized_and_large_gzip_payloads() {
        let Some(conn) = coverage_redis().await else {
            return;
        };
        let event_id = next_event_id();
        let mut cfg = test_config();
        cfg.max_value_bytes = 1;
        cfg.batch_max_value_bytes = 1;
        let cache = test_cache(&conn, &cfg);

        let oversized = cache
            .get_or_fetch_json_bytes("jp", event_id, "oversized".into(), 60, async {
                Ok(TypedCachePayload { items: vec![1, 2] })
            })
            .await
            .unwrap();
        assert!(!oversized.is_empty());

        let batch = cache
            .get_or_fetch_batch_encoded_json(
                "jp",
                event_id,
                "oversized-batch".into(),
                60,
                true,
                async { Ok(Bytes::from(vec![b'x'; 64])) },
            )
            .await
            .unwrap();
        assert_eq!(batch.encoding, CachedJsonEncoding::Gzip);

        let large = cache
            .get_or_fetch_encoded_json("jp", event_id, "large-gzip".into(), 0, true, async {
                Ok(TypedCachePayload {
                    items: (0..20_000).collect(),
                })
            })
            .await
            .unwrap();
        assert_eq!(large.encoding, CachedJsonEncoding::Gzip);
        assert!(large.bytes.len() > 100);
    }
}
