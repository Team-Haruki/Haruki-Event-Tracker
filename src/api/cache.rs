use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
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
    ReplayOverview,
}

#[derive(Clone)]
pub struct CachedJson {
    pub bytes: Bytes,
    pub encoding: CachedJsonEncoding,
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
        }
    }

    fn gzip(bytes: Bytes) -> Self {
        Self {
            bytes,
            encoding: CachedJsonEncoding::Gzip,
        }
    }
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
                validate_cached_bytes: None,
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
        if let Some(bytes) = self.l1.get_value(&key).await {
            if cached_bytes_are_valid(options, &bytes) {
                incr(&CACHE_STATS.l1_hit);
                tracing::debug!(cache_status = "static_l1_hit", "api static cache L1 hit");
                return Ok(bytes);
            }
            tracing::warn!(
                cache_status = "static_l1_invalid",
                "api static cache L1 invalid"
            );
        }

        self.lookup_with_singleflight(
            static_lookup_flight_key(server, event_id, &suffix),
            options,
            async {
                match self.read_l2_value(&key).await {
                    Ok(L2ValueRead::Hit(bytes)) => {
                        if cached_bytes_are_valid(options, &bytes) {
                            incr(&CACHE_STATS.l2_hit);
                            tracing::debug!(
                                cache_status = "static_l2_hit",
                                "api static cache L2 hit"
                            );
                            self.store_l1_value(key, bytes.clone()).await;
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
        let control_key = control_cache_key(server, event_id);
        if ttl_secs == 0 {
            return fetch.await;
        }

        if let Some(control) = self.l1.get_control(&control_key).await {
            incr(&CACHE_STATS.l1_control_hit);
            if control.dirty {
                incr(&CACHE_STATS.dirty_bypass);
                tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
                return self
                    .fetch_bytes_with_singleflight(
                        dirty_flight_key(server, event_id, control.epoch, &suffix),
                        fetch,
                        None,
                        options,
                    )
                    .await;
            }
            let key = value_key(server, event_id, control.epoch, &suffix);
            if let Some(bytes) = self.l1.get_value(&key).await {
                if cached_bytes_are_valid(options, &bytes) {
                    incr(&CACHE_STATS.l1_hit);
                    if options.is_batch {
                        incr(&CACHE_STATS.batch_l1_hit);
                    }
                    tracing::debug!(
                        cache_status = cache_status(options, "l1_hit"),
                        "api cache L1 hit"
                    );
                    return Ok(bytes);
                }
                tracing::warn!(cache_status = "l1_invalid", "api cache L1 invalid");
            }
            return self
                .lookup_with_singleflight(
                    lookup_flight_key(server, event_id, control.epoch, &suffix),
                    options,
                    async {
                        match self.read_l2_value(&key).await {
                            Ok(L2ValueRead::Hit(bytes)) => {
                                if cached_bytes_are_valid(options, &bytes) {
                                    incr(&CACHE_STATS.l2_hit);
                                    if options.is_batch {
                                        incr(&CACHE_STATS.batch_l2_hit);
                                    }
                                    tracing::debug!(
                                        cache_status = cache_status(options, "l2_hit"),
                                        "api cache L2 hit"
                                    );
                                    self.store_l1_value(key, bytes.clone()).await;
                                    Ok(bytes)
                                } else {
                                    tracing::warn!(
                                        cache_status = "l2_invalid",
                                        "api cache L2 invalid, refetching"
                                    );
                                    self.fetch_and_maybe_cache_bytes(
                                        fetch,
                                        Some(CacheWriteContext {
                                            server: server.to_owned(),
                                            event_id,
                                            epoch: control.epoch,
                                            value_key: key,
                                            negative_key: negative_key(
                                                server,
                                                event_id,
                                                control.epoch,
                                                &suffix,
                                            ),
                                            ttl_secs,
                                        }),
                                        options,
                                    )
                                    .await
                                }
                            }
                            Ok(L2ValueRead::NotFound) => {
                                incr(&CACHE_STATS.l2_not_found);
                                tracing::debug!(
                                    cache_status = "l2_not_found",
                                    "api cache negative hit"
                                );
                                Err(ApiError::NotFound)
                            }
                            Ok(L2ValueRead::Miss) => {
                                incr(&CACHE_STATS.l2_miss);
                                if options.is_batch {
                                    incr(&CACHE_STATS.batch_miss);
                                }
                                tracing::debug!(
                                    cache_status = cache_status(options, "l2_miss"),
                                    "api cache miss"
                                );
                                self.fetch_and_maybe_cache_bytes(
                                    fetch,
                                    Some(CacheWriteContext {
                                        server: server.to_owned(),
                                        event_id,
                                        epoch: control.epoch,
                                        value_key: key,
                                        negative_key: negative_key(
                                            server,
                                            event_id,
                                            control.epoch,
                                            &suffix,
                                        ),
                                        ttl_secs,
                                    }),
                                    options,
                                )
                                .await
                            }
                            Err(err) => {
                                incr(&CACHE_STATS.l2_timeout);
                                tracing::warn!(%err, "api cache value read failed");
                                fetch.await
                            }
                        }
                    },
                )
                .await;
        }

        self.lookup_with_singleflight(
            lookup_flight_key(server, event_id, -1, &suffix),
            options,
            async {
                match self.read_l2_combined(server, event_id, &suffix).await {
                    Ok(L2CombinedRead::Dirty { epoch }) => {
                        self.store_l1_control(control_key, epoch, true).await;
                        incr(&CACHE_STATS.dirty_bypass);
                        tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
                        self.fetch_bytes_with_singleflight(
                            dirty_flight_key(server, event_id, epoch, &suffix),
                            fetch,
                            None,
                            options,
                        )
                        .await
                    }
                    Ok(L2CombinedRead::Hit { epoch, key, bytes }) => {
                        self.store_l1_control(control_key, epoch, false).await;
                        if cached_bytes_are_valid(options, &bytes) {
                            self.store_l1_value(key, bytes.clone()).await;
                            incr(&CACHE_STATS.l2_hit);
                            if options.is_batch {
                                incr(&CACHE_STATS.batch_l2_hit);
                            }
                            tracing::debug!(
                                cache_status = cache_status(options, "l2_hit"),
                                "api cache L2 hit"
                            );
                            Ok(bytes)
                        } else {
                            tracing::warn!(
                                cache_status = "l2_invalid",
                                "api cache L2 invalid, refetching"
                            );
                            self.fetch_and_maybe_cache_bytes(
                                fetch,
                                Some(CacheWriteContext {
                                    server: server.to_owned(),
                                    event_id,
                                    epoch,
                                    value_key: key,
                                    negative_key: negative_key(server, event_id, epoch, &suffix),
                                    ttl_secs,
                                }),
                                options,
                            )
                            .await
                        }
                    }
                    Ok(L2CombinedRead::NotFound { epoch }) => {
                        self.store_l1_control(control_key, epoch, false).await;
                        incr(&CACHE_STATS.l2_not_found);
                        tracing::debug!(cache_status = "l2_not_found", "api cache negative hit");
                        Err(ApiError::NotFound)
                    }
                    Ok(L2CombinedRead::Miss { epoch, key }) => {
                        self.store_l1_control(control_key, epoch, false).await;
                        incr(&CACHE_STATS.l2_miss);
                        if options.is_batch {
                            incr(&CACHE_STATS.batch_miss);
                        }
                        tracing::debug!(
                            cache_status = cache_status(options, "l2_miss"),
                            "api cache miss"
                        );
                        self.fetch_and_maybe_cache_bytes(
                            fetch,
                            Some(CacheWriteContext {
                                server: server.to_owned(),
                                event_id,
                                epoch,
                                value_key: key,
                                negative_key: negative_key(server, event_id, epoch, &suffix),
                                ttl_secs,
                            }),
                            options,
                        )
                        .await
                    }
                    Err(err) => {
                        incr(&CACHE_STATS.l2_timeout);
                        tracing::warn!(%err, "api cache combined read failed");
                        fetch.await
                    }
                }
            },
        )
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
        let control_key = control_cache_key(server, event_id);
        if ttl_secs == 0 {
            return self.encode_response(fetch.await?, None, options).await;
        }

        if let Some(control) = self.l1.get_control(&control_key).await {
            incr(&CACHE_STATS.l1_control_hit);
            if control.dirty {
                incr(&CACHE_STATS.dirty_bypass);
                tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
                return self
                    .fetch_encoded_with_singleflight(
                        gzip_flight_key(server, event_id, control.epoch, &suffix),
                        fetch,
                        None,
                        options,
                    )
                    .await;
            }

            let key = value_key(server, event_id, control.epoch, &suffix);
            let gzip = gzip_key(&key);
            if let Some(bytes) = self.l1.get_value(&gzip).await {
                incr(&CACHE_STATS.l1_hit);
                if options.is_batch {
                    incr(&CACHE_STATS.batch_l1_hit);
                }
                tracing::debug!(
                    cache_status = cache_status(options, "l1_gzip_hit"),
                    "api cache L1 gzip hit"
                );
                return Ok(CachedJson::gzip(bytes));
            }
            if let Some(bytes) = self.l1.get_value(&key).await {
                incr(&CACHE_STATS.l1_hit);
                if options.is_batch {
                    incr(&CACHE_STATS.batch_l1_hit);
                }
                tracing::debug!(
                    cache_status = cache_status(options, "l1_hit"),
                    "api cache L1 hit, building gzip"
                );
                return self
                    .encode_response(
                        bytes,
                        Some(CacheWriteContext {
                            server: server.to_owned(),
                            event_id,
                            epoch: control.epoch,
                            value_key: key,
                            negative_key: negative_key(server, event_id, control.epoch, &suffix),
                            ttl_secs,
                        }),
                        options,
                    )
                    .await;
            }

            return self
                .lookup_encoded_with_singleflight(
                    gzip_lookup_flight_key(server, event_id, control.epoch, &suffix),
                    options,
                    async {
                        match self.read_l2_encoded(&key, &gzip).await {
                            Ok(L2EncodedRead::Gzip(bytes)) => {
                                incr(&CACHE_STATS.l2_hit);
                                if options.is_batch {
                                    incr(&CACHE_STATS.batch_l2_hit);
                                }
                                tracing::debug!(
                                    cache_status = cache_status(options, "l2_gzip_hit"),
                                    "api cache L2 gzip hit"
                                );
                                self.store_l1_value(gzip, bytes.clone()).await;
                                Ok(CachedJson::gzip(bytes))
                            }
                            Ok(L2EncodedRead::Identity(bytes)) => {
                                incr(&CACHE_STATS.l2_hit);
                                if options.is_batch {
                                    incr(&CACHE_STATS.batch_l2_hit);
                                }
                                tracing::debug!(
                                    cache_status = cache_status(options, "l2_hit"),
                                    "api cache L2 hit, building gzip"
                                );
                                self.store_l1_value(key.clone(), bytes.clone()).await;
                                self.encode_response(
                                    bytes,
                                    Some(CacheWriteContext {
                                        server: server.to_owned(),
                                        event_id,
                                        epoch: control.epoch,
                                        value_key: key,
                                        negative_key: negative_key(
                                            server,
                                            event_id,
                                            control.epoch,
                                            &suffix,
                                        ),
                                        ttl_secs,
                                    }),
                                    options,
                                )
                                .await
                            }
                            Ok(L2EncodedRead::NotFound) => {
                                incr(&CACHE_STATS.l2_not_found);
                                tracing::debug!(
                                    cache_status = "l2_not_found",
                                    "api cache negative hit"
                                );
                                Err(ApiError::NotFound)
                            }
                            Ok(L2EncodedRead::Miss) => {
                                incr(&CACHE_STATS.l2_miss);
                                if options.is_batch {
                                    incr(&CACHE_STATS.batch_miss);
                                }
                                tracing::debug!(
                                    cache_status = cache_status(options, "l2_miss"),
                                    "api cache miss"
                                );
                                self.fetch_and_maybe_cache_encoded(
                                    fetch,
                                    Some(CacheWriteContext {
                                        server: server.to_owned(),
                                        event_id,
                                        epoch: control.epoch,
                                        value_key: key,
                                        negative_key: negative_key(
                                            server,
                                            event_id,
                                            control.epoch,
                                            &suffix,
                                        ),
                                        ttl_secs,
                                    }),
                                    options,
                                )
                                .await
                            }
                            Err(err) => {
                                incr(&CACHE_STATS.l2_timeout);
                                tracing::warn!(%err, "api cache encoded read failed");
                                self.encode_response(fetch.await?, None, options).await
                            }
                        }
                    },
                )
                .await;
        }

        self.lookup_encoded_with_singleflight(
            gzip_lookup_flight_key(server, event_id, -1, &suffix),
            options,
            async {
                match self.read_l2_combined(server, event_id, &suffix).await {
                    Ok(L2CombinedRead::Dirty { epoch }) => {
                        self.store_l1_control(control_key, epoch, true).await;
                        incr(&CACHE_STATS.dirty_bypass);
                        tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
                        self.fetch_encoded_with_singleflight(
                            gzip_flight_key(server, event_id, epoch, &suffix),
                            fetch,
                            None,
                            options,
                        )
                        .await
                    }
                    Ok(L2CombinedRead::Hit { epoch, key, bytes }) => {
                        self.store_l1_control(control_key, epoch, false).await;
                        self.store_l1_value(key.clone(), bytes.clone()).await;
                        incr(&CACHE_STATS.l2_hit);
                        if options.is_batch {
                            incr(&CACHE_STATS.batch_l2_hit);
                        }
                        tracing::debug!(
                            cache_status = cache_status(options, "l2_hit"),
                            "api cache L2 hit"
                        );
                        self.encode_response(
                            bytes,
                            Some(CacheWriteContext {
                                server: server.to_owned(),
                                event_id,
                                epoch,
                                value_key: key,
                                negative_key: negative_key(server, event_id, epoch, &suffix),
                                ttl_secs,
                            }),
                            options,
                        )
                        .await
                    }
                    Ok(L2CombinedRead::NotFound { epoch }) => {
                        self.store_l1_control(control_key, epoch, false).await;
                        incr(&CACHE_STATS.l2_not_found);
                        tracing::debug!(cache_status = "l2_not_found", "api cache negative hit");
                        Err(ApiError::NotFound)
                    }
                    Ok(L2CombinedRead::Miss { epoch, key }) => {
                        self.store_l1_control(control_key, epoch, false).await;
                        incr(&CACHE_STATS.l2_miss);
                        if options.is_batch {
                            incr(&CACHE_STATS.batch_miss);
                        }
                        tracing::debug!(
                            cache_status = cache_status(options, "l2_miss"),
                            "api cache miss"
                        );
                        self.fetch_and_maybe_cache_encoded(
                            fetch,
                            Some(CacheWriteContext {
                                server: server.to_owned(),
                                event_id,
                                epoch,
                                value_key: key,
                                negative_key: negative_key(server, event_id, epoch, &suffix),
                                ttl_secs,
                            }),
                            options,
                        )
                        .await
                    }
                    Err(err) => {
                        incr(&CACHE_STATS.l2_timeout);
                        tracing::warn!(%err, "api cache combined read failed");
                        self.encode_response(fetch.await?, None, options).await
                    }
                }
            },
        )
        .await
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
        match self.singleflight.begin(flight_key.clone()).await {
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
            Flight::Owner(entry) => {
                let guard = SingleFlightOwnerGuard::new(
                    self.singleflight.clone(),
                    flight_key.clone(),
                    entry,
                );
                let result = lookup.await;
                let shared = shared_fetch_bytes_result(&result);
                guard.finish(shared).await;
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
        match self.singleflight.begin(flight_key.clone()).await {
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
            Flight::Owner(entry) => {
                let guard = SingleFlightOwnerGuard::new(
                    self.singleflight.clone(),
                    flight_key.clone(),
                    entry,
                );
                let result = self
                    .fetch_and_maybe_cache_bytes(fetch, write_context, options)
                    .await;
                let shared = shared_fetch_bytes_result(&result);
                guard.finish(shared).await;
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
        match self.singleflight.begin(flight_key.clone()).await {
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
            Flight::Owner(entry) => {
                let guard = SingleFlightOwnerGuard::new(
                    self.singleflight.clone(),
                    flight_key.clone(),
                    entry,
                );
                let result = lookup.await;
                let shared = shared_cached_json_result(&result);
                guard.finish(shared).await;
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
        match self.singleflight.begin(flight_key.clone()).await {
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
            Flight::Owner(entry) => {
                let guard = SingleFlightOwnerGuard::new(
                    self.singleflight.clone(),
                    flight_key.clone(),
                    entry,
                );
                let result = self
                    .fetch_and_maybe_cache_encoded(fetch, write_context, options)
                    .await;
                let shared = shared_cached_json_result(&result);
                guard.finish(shared).await;
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
            Ok(true) => {
                self.store_l1_value(ctx.value_key.clone(), bytes.clone())
                    .await
            }
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
            Ok(()) => self.store_l1_value(key, bytes.clone()).await,
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
        let encoded = self
            .encode_response(bytes.clone(), Some(ctx.clone()), options)
            .await?;
        match self
            .write_l2_if_clean(&ctx, &ctx.value_key, bytes.clone(), ctx.ttl_secs)
            .await
        {
            Ok(true) => {
                self.store_l1_value(ctx.value_key.clone(), bytes).await;
                if encoded.encoding == CachedJsonEncoding::Gzip {
                    self.store_l1_value(gzip_key(&ctx.value_key), encoded.bytes.clone())
                        .await;
                }
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, "api cache write failed"),
        }
        Ok(encoded)
    }

    async fn encode_response(
        &self,
        bytes: Bytes,
        write_context: Option<CacheWriteContext>,
        _options: CacheOptions,
    ) -> Result<CachedJson, ApiError> {
        if bytes.len() < self.cfg.precompress_min_bytes {
            return Ok(CachedJson::identity(bytes));
        }
        let gzip = gzip_bytes(&bytes, self.cfg.gzip_level)?;
        if let Some(ctx) = write_context {
            match self
                .write_l2_if_clean(&ctx, &gzip_key(&ctx.value_key), gzip.clone(), ctx.ttl_secs)
                .await
            {
                Ok(true) => {
                    self.store_l1_value(gzip_key(&ctx.value_key), gzip.clone())
                        .await
                }
                Ok(false) => {}
                Err(err) => tracing::warn!(%err, "api cache gzip write failed"),
            }
        }
        Ok(CachedJson::gzip(gzip))
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
        let mut invocation = script.prepare_invoke();
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

    async fn read_l2_value(&self, key: &str) -> Result<L2ValueRead, redis::RedisError> {
        let mut conn = self.conns.connection();
        let mut pipe = redis::pipe();
        pipe.get(key).exists(format!("{key}:not_found"));
        let fut = pipe.query_async::<(Option<Vec<u8>>, bool)>(&mut conn);
        let (value, negative) = self.with_timeout(fut).await?;
        if let Some(bytes) = value {
            Ok(L2ValueRead::Hit(Bytes::from(bytes)))
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
        let script = redis::Script::new(WRITE_SCRIPT);
        let mut invocation = script.prepare_invoke();
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

    async fn store_l1_value(&self, key: String, bytes: Bytes) {
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
    bytes: Bytes,
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

    async fn get_value(&self, key: &str) -> Option<Bytes> {
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
    Hit(Bytes),
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
struct CacheOptions {
    max_value_bytes: usize,
    is_batch: bool,
    validate_cached_bytes: Option<fn(&Bytes) -> bool>,
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
    Value(CachedJson),
    NotFound,
}

struct SingleFlightOwnerGuard {
    singleflight: SingleFlight,
    key: String,
    entry: Option<Arc<InFlightEntry>>,
}

impl SingleFlightOwnerGuard {
    fn new(singleflight: SingleFlight, key: String, entry: Arc<InFlightEntry>) -> Self {
        Self {
            singleflight,
            key,
            entry: Some(entry),
        }
    }

    async fn finish(mut self, result: Option<SharedFetchResult>) {
        if let Some(entry) = self.entry.take() {
            self.singleflight.finish(&self.key, entry, result).await;
        }
    }
}

impl Drop for SingleFlightOwnerGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            let singleflight = self.singleflight.clone();
            let key = self.key.clone();
            tokio::spawn(async move {
                singleflight.finish(&key, entry, None).await;
            });
        }
    }
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

    async fn wait_bytes(entry: Arc<InFlightEntry>) -> Option<Result<Bytes, ApiError>> {
        loop {
            let notified = entry.notify.notified();
            {
                let state = entry.state.lock().await;
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
                let state = entry.state.lock().await;
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

fn static_value_key(server: &str, event_id: i64, suffix: &str) -> String {
    format!("{base}:static:{suffix}", base = base_key(server, event_id))
}

fn negative_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!("{}:not_found", value_key(server, event_id, epoch, suffix))
}

fn gzip_key(value_key: &str) -> String {
    format!("{value_key}:gz")
}

fn dirty_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!(
        "{base}:dirty:v{epoch}:{suffix}",
        base = base_key(server, event_id)
    )
}

fn lookup_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!(
        "{base}:lookup:v{epoch}:{suffix}",
        base = base_key(server, event_id)
    )
}

fn static_lookup_flight_key(server: &str, event_id: i64, suffix: &str) -> String {
    format!(
        "{base}:static_lookup:{suffix}",
        base = base_key(server, event_id)
    )
}

fn gzip_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!(
        "{base}:gzip:v{epoch}:{suffix}",
        base = base_key(server, event_id)
    )
}

fn gzip_lookup_flight_key(server: &str, event_id: i64, epoch: i64, suffix: &str) -> String {
    format!(
        "{base}:gzip_lookup:v{epoch}:{suffix}",
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
    use serde::Deserialize;

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

    #[tokio::test]
    async fn l1_value_hit_returns_cached_bytes() {
        let l1 = L1Cache::new(16);
        l1.insert_value(
            "value".to_owned(),
            L1Value {
                bytes: Bytes::from_static(b"cached"),
                expires_at: Instant::now() + Duration::from_secs(1),
            },
        )
        .await;

        assert_eq!(
            l1.get_value("value").await,
            Some(Bytes::from_static(b"cached"))
        );
    }

    #[tokio::test]
    async fn l1_value_expiry_removes_cached_bytes() {
        let l1 = L1Cache::new(16);
        l1.insert_value(
            "value".to_owned(),
            L1Value {
                bytes: Bytes::from_static(b"stale"),
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
                bytes: Bytes::from_static(b"cached"),
                expires_at: Instant::now() + Duration::from_secs(1),
            },
        )
        .await;

        assert_eq!(l1.get_value("value").await, None);
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
        let owner = match singleflight.begin(key.clone()).await {
            Flight::Owner(entry) => entry,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()).await {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };

        let task =
            tokio::spawn(async move { SingleFlight::wait_bytes(waiter).await.unwrap().unwrap() });
        singleflight
            .finish(
                &key,
                owner,
                Some(SharedFetchResult::Value(CachedJson::identity(
                    Bytes::from_static(b"{\"ok\":true}"),
                ))),
            )
            .await;

        assert_eq!(task.await.unwrap(), Bytes::from_static(b"{\"ok\":true}"));
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

        let task = tokio::spawn(async move { SingleFlight::wait_bytes(waiter).await.unwrap() });
        singleflight
            .finish(&key, owner, Some(SharedFetchResult::NotFound))
            .await;

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
        let owner = match singleflight.begin(key.clone()).await {
            Flight::Owner(entry) => entry,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let waiter = match singleflight.begin(key.clone()).await {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };

        let task = tokio::spawn(async move {
            SingleFlight::wait_cached_json(waiter)
                .await
                .unwrap()
                .unwrap()
        });
        singleflight
            .finish(
                &key,
                owner,
                Some(SharedFetchResult::Value(CachedJson::gzip(
                    Bytes::from_static(b"gzipped"),
                ))),
            )
            .await;

        let shared = task.await.unwrap();
        assert_eq!(shared.encoding, CachedJsonEncoding::Gzip);
        assert_eq!(shared.bytes, Bytes::from_static(b"gzipped"));
    }

    #[tokio::test]
    async fn singleflight_owner_drop_releases_waiters_and_key() {
        let singleflight = SingleFlight::default();
        let key = "trace:ranks:1,2,3".to_owned();
        let owner = match singleflight.begin(key.clone()).await {
            Flight::Owner(entry) => entry,
            Flight::Waiter(_) => panic!("first caller should own the flight"),
        };
        let guard = SingleFlightOwnerGuard::new(singleflight.clone(), key.clone(), owner);
        let waiter = match singleflight.begin(key.clone()).await {
            Flight::Waiter(entry) => entry,
            Flight::Owner(_) => panic!("second caller should wait on the flight"),
        };
        let task = tokio::spawn(async move { SingleFlight::wait_bytes(waiter).await });

        drop(guard);

        assert!(task.await.unwrap().is_none());
        match singleflight.begin(key).await {
            Flight::Owner(_) => {}
            Flight::Waiter(_) => panic!("dropped owner should remove in-flight key"),
        }
    }
}
