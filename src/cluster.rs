//! Writer → reader update fan-out for the cluster deployment.
//!
//! A `writer` process owns every tracker daemon and publishes one
//! `UpdateEvent` per committed flush on an in-process `UpdateBus`; the
//! `/internal/updates` WebSocket endpoint streams the bus to any reader
//! that presents the cluster token. A `reader` process dials that endpoint
//! (`subscriber`), and on every event bumps its *local* API-cache epoch and
//! pushes the realtime `updated` notification to its own WebSocket clients.
//! Readers own reconnection; the writer keeps no per-peer state.

pub mod subscriber;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::model::enums::SekaiServerRegion;

/// Broadcast capacity. Readers that fall this far behind get a fresh
/// `hello` and resync by over-invalidating, so the bound only limits memory.
const BUS_CAPACITY: usize = 1024;

/// Wire format of `/internal/updates`. `seq` is a per-writer-process
/// monotonic counter; a gap tells the reader it missed events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum StreamMessage {
    Hello {
        seq: u64,
    },
    Updated {
        seq: u64,
        server: SekaiServerRegion,
        event_id: i64,
        timestamp: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lsn: Option<String>,
    },
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateEvent {
    pub seq: u64,
    pub server: SekaiServerRegion,
    pub event_id: i64,
    pub timestamp: i64,
    /// Primary WAL position after the commit (Postgres writers only), so a
    /// reader on a streaming replica can wait for replay before
    /// invalidating.
    pub lsn: Option<String>,
}

impl UpdateEvent {
    pub fn into_message(self) -> StreamMessage {
        StreamMessage::Updated {
            seq: self.seq,
            server: self.server,
            event_id: self.event_id,
            timestamp: self.timestamp,
            lsn: self.lsn,
        }
    }
}

#[derive(Clone)]
pub struct UpdateBus {
    inner: Arc<BusInner>,
}

struct BusInner {
    tx: broadcast::Sender<UpdateEvent>,
    seq: AtomicU64,
}

impl Default for UpdateBus {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            inner: Arc::new(BusInner {
                tx,
                seq: AtomicU64::new(0),
            }),
        }
    }

    /// Assigns the next sequence number and fans the event out. Returns the
    /// sequence used. No subscribers is not an error — a writer keeps
    /// tracking whether or not any reader is attached.
    pub fn publish(
        &self,
        server: SekaiServerRegion,
        event_id: i64,
        timestamp: i64,
        lsn: Option<String>,
    ) -> u64 {
        let seq = self.inner.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.inner.tx.send(UpdateEvent {
            seq,
            server,
            event_id,
            timestamp,
            lsn,
        });
        seq
    }

    pub fn current_seq(&self) -> u64 {
        self.inner.seq.load(Ordering::Acquire)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UpdateEvent> {
        self.inner.tx.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.inner.tx.receiver_count()
    }
}

/// Reader-side view of the update stream, exposed through `/readyz`.
#[derive(Debug, Default)]
pub struct ClusterLink {
    connected: AtomicBool,
    last_seq: AtomicU64,
    last_update_at: AtomicI64,
    reconnects: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterLinkStatus {
    pub connected: bool,
    pub last_seq: u64,
    pub last_update_at: i64,
    pub reconnects: u64,
}

impl ClusterLink {
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Release);
    }

    pub fn record_update(&self, seq: u64, timestamp: i64) {
        self.last_seq.store(seq, Ordering::Release);
        self.last_update_at.store(timestamp, Ordering::Release);
    }

    pub fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::AcqRel);
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq.load(Ordering::Acquire)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn status(&self) -> ClusterLinkStatus {
        ClusterLinkStatus {
            connected: self.is_connected(),
            last_seq: self.last_seq(),
            last_update_at: self.last_update_at.load(Ordering::Acquire),
            reconnects: self.reconnects.load(Ordering::Acquire),
        }
    }
}

/// Constant-time bearer token check shared by the cloud group and the
/// internal endpoints. Length differences leak nothing useful here (tokens
/// are random and fixed-length in practice) but are still folded in.
pub fn token_matches(candidate: &str, expected: &str) -> bool {
    let a = candidate.as_bytes();
    let b = expected.as_bytes();
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0 && !expected.is_empty()
}

pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_assigns_monotonic_sequence_and_delivers() {
        let bus = UpdateBus::new();
        let mut rx = bus.subscribe();
        assert_eq!(bus.current_seq(), 0);
        let seq = bus.publish(
            SekaiServerRegion::Cn,
            179,
            1_700_000_000,
            Some("0/1".into()),
        );
        assert_eq!(seq, 1);
        assert_eq!(bus.current_seq(), 1);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.seq, 1);
        assert_eq!(event.server, SekaiServerRegion::Cn);
        assert_eq!(event.event_id, 179);
        assert_eq!(event.lsn.as_deref(), Some("0/1"));
        // Publishing with nobody listening must not fail.
        drop(rx);
        assert_eq!(bus.publish(SekaiServerRegion::Jp, 1, 0, None), 2);
    }

    #[test]
    fn stream_messages_round_trip_with_camel_case_fields() {
        let msg = UpdateEvent {
            seq: 7,
            server: SekaiServerRegion::Jp,
            event_id: 200,
            timestamp: 42,
            lsn: None,
        }
        .into_message();
        let text = sonic_rs::to_string(&msg).unwrap();
        assert_eq!(
            text,
            r#"{"type":"updated","seq":7,"server":"jp","eventId":200,"timestamp":42}"#
        );
        let back: StreamMessage = sonic_rs::from_str(&text).unwrap();
        assert_eq!(back, msg);
        let hello: StreamMessage = sonic_rs::from_str(r#"{"type":"hello","seq":3}"#).unwrap();
        assert_eq!(hello, StreamMessage::Hello { seq: 3 });
        let ping: StreamMessage = sonic_rs::from_str(r#"{"type":"ping"}"#).unwrap();
        assert_eq!(ping, StreamMessage::Ping);
    }

    #[test]
    fn link_status_reflects_updates() {
        let link = ClusterLink::default();
        assert!(!link.is_connected());
        link.set_connected(true);
        link.record_update(9, 123);
        link.record_reconnect();
        let status = link.status();
        assert!(status.connected);
        assert_eq!(status.last_seq, 9);
        assert_eq!(status.last_update_at, 123);
        assert_eq!(status.reconnects, 1);
    }

    #[test]
    fn token_comparison_rejects_empty_and_mismatched() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("ab", "abc"));
        assert!(!token_matches("", ""));
        let mut headers = axum::http::HeaderMap::new();
        assert!(bearer_token(&headers).is_none());
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer  tok ".parse().unwrap(),
        );
        assert_eq!(bearer_token(&headers), Some("tok"));
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Basic x".parse().unwrap(),
        );
        assert!(bearer_token(&headers).is_none());
    }
}
