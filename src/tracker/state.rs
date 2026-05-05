//! Redis state persistence for the tracker.
//!
//! All keys live under `haruki:tracker:<server>:<event>:<suffix>` —
//! **byte-compatible with the Go version** so a Rust daemon can resume
//! mid-event from state previously written by the Go daemon. The two
//! suffixes we read/write are:
//!
//! - `rank_state` — `HSET <key> <rank> <json{u,s}>`, 14 day TTL
//! - `ended` — `SET <key> "true"`, 30 day TTL
//!
//! Go also wrote a `user_state` hash but never read it back into any diff
//! logic, so we skip it.

use std::collections::HashMap;

use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use crate::model::enums::SekaiServerRegion;
use crate::model::tracker::RankState;

const RANK_STATE_TTL_SECS: i64 = 14 * 24 * 60 * 60;
const ENDED_FLAG_TTL_SECS: i64 = 30 * 24 * 60 * 60;

pub fn redis_key(server: SekaiServerRegion, event_id: i64, suffix: &str) -> String {
    format!("haruki:tracker:{server}:{event_id}:{suffix}")
}

#[tracing::instrument(skip(conn), fields(server = %server, event_id))]
pub async fn load_rank_state(
    conn: &mut ConnectionManager,
    server: SekaiServerRegion,
    event_id: i64,
) -> Result<HashMap<i64, RankState>, redis::RedisError> {
    let key = redis_key(server, event_id, "rank_state");
    let raw: HashMap<String, String> = conn.hgetall(&key).await?;
    let mut out = HashMap::with_capacity(raw.len());
    for (k, v) in raw {
        let Ok(rank) = k.parse::<i64>() else {
            continue;
        };
        match sonic_rs::from_str::<RankState>(&v) {
            Ok(state) => {
                out.insert(rank, state);
            }
            Err(err) => {
                tracing::warn!(rank, %err, "skipping unparseable rank_state entry");
            }
        }
    }
    Ok(out)
}

#[tracing::instrument(skip(conn, changed), fields(server = %server, event_id, n = changed.len()))]
pub async fn save_rank_state(
    conn: &mut ConnectionManager,
    server: SekaiServerRegion,
    event_id: i64,
    changed: &HashMap<i64, RankState>,
) -> Result<(), redis::RedisError> {
    if changed.is_empty() {
        return Ok(());
    }
    let key = redis_key(server, event_id, "rank_state");
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(changed.len());
    for (rank, state) in changed {
        let json = sonic_rs::to_string(state).map_err(|e| {
            redis::RedisError::from((
                redis::ErrorKind::UnexpectedReturnType,
                "encode rank_state",
                e.to_string(),
            ))
        })?;
        pairs.push((rank.to_string(), json));
    }
    let mut pipe = redis::pipe();
    pipe.hset_multiple(&key, &pairs)
        .ignore()
        .expire(&key, RANK_STATE_TTL_SECS)
        .ignore();
    pipe.query_async::<()>(conn).await
}

#[tracing::instrument(skip(conn), fields(server = %server, event_id))]
pub async fn check_event_ended_flag(
    conn: &mut ConnectionManager,
    server: SekaiServerRegion,
    event_id: i64,
) -> Result<bool, redis::RedisError> {
    let key = redis_key(server, event_id, "ended");
    let val: Option<String> = conn.get(&key).await?;
    Ok(val.as_deref() == Some("true"))
}

#[tracing::instrument(skip(conn), fields(server = %server, event_id))]
pub async fn set_event_ended_flag(
    conn: &mut ConnectionManager,
    server: SekaiServerRegion,
    event_id: i64,
) -> Result<(), redis::RedisError> {
    let key = redis_key(server, event_id, "ended");
    conn.set_ex::<_, _, ()>(&key, "true", ENDED_FLAG_TTL_SECS as u64)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_format_matches_go() {
        assert_eq!(
            redis_key(SekaiServerRegion::Jp, 137, "rank_state"),
            "haruki:tracker:jp:137:rank_state"
        );
        assert_eq!(
            redis_key(SekaiServerRegion::En, 200, "ended"),
            "haruki:tracker:en:200:ended"
        );
    }
}
