use std::future::Future;

use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::error::ApiError;
use crate::config::ApiCacheConfig;

const DIRTY_TTL_SECS: u64 = 300;

#[derive(Clone)]
pub struct ApiCache {
    conn: ConnectionManager,
    cfg: ApiCacheConfig,
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
        Self { conn, cfg }
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
            Ok(true) => return fetch.await,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(%err, "api cache dirty read failed");
                return fetch.await;
            }
        }

        let key = value_key(server, event_id, epoch, &suffix);
        match conn.get::<_, Option<Vec<u8>>>(&key).await {
            Ok(Some(bytes)) => match sonic_rs::from_slice::<T>(&bytes) {
                Ok(value) => return Ok(value),
                Err(err) => tracing::warn!(%err, "api cache decode failed"),
            },
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, "api cache read failed"),
        }

        let value = fetch.await?;
        let bytes = match sonic_rs::to_vec(&value) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(%err, "api cache encode failed");
                return Ok(value);
            }
        };
        if bytes.len() > self.cfg.max_value_bytes {
            return Ok(value);
        }

        let mut conn = self.conn.clone();
        let still_clean = matches!(is_dirty(&mut conn, server, event_id).await, Ok(false));
        let same_epoch =
            matches!(read_epoch(&mut conn, server, event_id).await, Ok(next) if next == epoch);
        if still_clean
            && same_epoch
            && let Err(err) = conn.set_ex::<_, _, ()>(&key, bytes, ttl_secs).await
        {
            tracing::warn!(%err, "api cache write failed");
        }
        Ok(value)
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

fn epoch_key(server: &str, event_id: i64) -> String {
    format!("{base}:epoch", base = base_key(server, event_id))
}

fn dirty_key(server: &str, event_id: i64) -> String {
    format!("{base}:dirty", base = base_key(server, event_id))
}

fn base_key(server: &str, event_id: i64) -> String {
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
            value_key("jp", 137, 3, "trace:rank:100"),
            "haruki:tracker:jp:137:api_cache:v3:trace:rank:100"
        );
        assert_eq!(
            dirty_key("en", 200),
            "haruki:tracker:en:200:api_cache:dirty"
        );
    }
}
