//! Redis-backed border-payload deduplication.
//!
//! `detect_cache(key, hash)` returns `Ok(true)` when the new SHA-256 of
//! the upstream border response equals what we last stored under `key`,
//! letting the tracker tick short-circuit the merge step (see
//! `tracker::diff::merge_rankings`). Mismatch (or first call) returns
//! `Ok(false)` and refreshes the cache.
//!
//! Mirrors `EventTrackerBase.detectCache` in `tracker/trackerbase.go:184`.
//! No TTL — the key is overwritten every miss and naturally garbage
//! collected when the event ends and the tracker stops touching it.

use std::fmt::Write as _;

use redis::AsyncCommands;
use redis::aio::ConnectionManager;

fn hex_of(hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in hash {
        // 02x is the same lowercase, zero-padded encoding Go produces with
        // `fmt.Sprintf("%x", hash)` — required for byte-equality across
        // the cutover.
        write!(s, "{b:02x}").expect("write to String");
    }
    s
}

#[tracing::instrument(skip(conn, hash), fields(key))]
pub async fn detect_cache(
    conn: &mut ConnectionManager,
    key: &str,
    hash: &[u8; 32],
) -> Result<bool, redis::RedisError> {
    let new_hex = hex_of(hash);
    let cached: Option<String> = conn.get(key).await?;
    match cached {
        Some(prev) if prev == new_hex => {
            tracing::debug!("border cache hit");
            Ok(true)
        }
        _ => {
            tracing::debug!("border cache miss, refreshing");
            conn.set::<_, _, ()>(key, new_hex).await?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_matches_go_format() {
        let hash: [u8; 32] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0xff,
        ];
        let s = hex_of(&hash);
        assert_eq!(
            s,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1eff"
        );
        assert_eq!(s.len(), 64);
    }
}
