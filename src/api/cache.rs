use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, Notify};

use crate::api::error::ApiError;
use crate::config::ApiCacheConfig;

const DIRTY_TTL_SECS: u64 = 300;

#[derive(Clone)]
pub struct ApiCache {
    conn: ConnectionManager,
    cfg: ApiCacheConfig,
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
    pub fn new(conn: ConnectionManager, cfg: ApiCacheConfig) -> Self {
        Self {
            conn,
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

        let mut conn = self.conn.clone();
        let epoch = match read_epoch(&mut conn, server, event_id).await {
            Ok(epoch) => epoch,
            Err(err) => {
                tracing::warn!(%err, "api cache epoch read failed");
                return fetch.await;
            }
        };
        match is_dirty(&mut conn, server, event_id).await {
            Ok(true) => {
                tracing::debug!(cache_status = "dirty_bypass", "api cache dirty bypass");
                return self
                    .fetch_with_singleflight(
                        dirty_flight_key(server, event_id, epoch, &suffix),
                        fetch,
                        None,
                    )
                    .await;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(%err, "api cache dirty read failed");
                return fetch.await;
            }
        }

        let key = value_key(server, event_id, epoch, &suffix);
        match conn.get::<_, Option<Vec<u8>>>(&key).await {
            Ok(Some(bytes)) => match sonic_rs::from_slice::<T>(&bytes) {
                Ok(value) => {
                    tracing::debug!(cache_status = "hit", "api cache hit");
                    return Ok(value);
                }
                Err(err) => tracing::warn!(%err, "api cache decode failed"),
            },
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, "api cache read failed"),
        }
        let negative_key = negative_key(server, event_id, epoch, &suffix);
        match conn.exists::<_, bool>(&negative_key).await {
            Ok(true) => {
                tracing::debug!(cache_status = "not_found_cached", "api cache negative hit");
                return Err(ApiError::NotFound);
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, "api cache negative read failed"),
        }

        tracing::debug!(cache_status = "miss", "api cache miss");
        self.fetch_with_singleflight(
            key.clone(),
            fetch,
            Some(CacheWriteContext {
                server: server.to_owned(),
                event_id,
                epoch,
                value_key: key,
                negative_key,
                ttl_secs,
            }),
        )
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
                let mut conn = self.conn.clone();
                let still_clean = matches!(
                    is_dirty(&mut conn, &ctx.server, ctx.event_id).await,
                    Ok(false)
                );
                let same_epoch = matches!(
                    read_epoch(&mut conn, &ctx.server, ctx.event_id).await,
                    Ok(next) if next == ctx.epoch
                );
                if still_clean
                    && same_epoch
                    && let Err(err) = conn
                        .set_ex::<_, _, ()>(&ctx.negative_key, "1", self.cfg.negative_ttl_secs)
                        .await
                {
                    tracing::warn!(%err, "api cache negative write failed");
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
        let mut conn = self.conn.clone();
        let still_clean = matches!(
            is_dirty(&mut conn, &ctx.server, ctx.event_id).await,
            Ok(false)
        );
        let same_epoch = matches!(read_epoch(&mut conn, &ctx.server, ctx.event_id).await, Ok(next) if next == ctx.epoch);
        if still_clean
            && same_epoch
            && let Err(err) = conn
                .set_ex::<_, _, ()>(&ctx.value_key, bytes, ctx.ttl_secs)
                .await
        {
            tracing::warn!(%err, "api cache write failed");
        }
        Ok(value)
    }
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

async fn read_epoch(
    conn: &mut ConnectionManager,
    server: &str,
    event_id: i64,
) -> Result<i64, redis::RedisError> {
    let epoch: Option<i64> = conn.get(epoch_key(server, event_id)).await?;
    Ok(epoch.unwrap_or(0))
}

async fn is_dirty(
    conn: &mut ConnectionManager,
    server: &str,
    event_id: i64,
) -> Result<bool, redis::RedisError> {
    conn.exists(dirty_key(server, event_id)).await
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
            dirty_key("en", 200),
            "haruki:tracker:en:200:api_cache:dirty"
        );
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
