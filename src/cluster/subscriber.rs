//! Reader side of the update stream: dial the writer, apply every
//! `updated` event to the local cache epoch and realtime hub, reconnect
//! forever with bounded backoff.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use redis::aio::ConnectionManager;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::api::cache::finish_event_update;
use crate::api::realtime::{RealtimeHub, RealtimeTopic};
use crate::cluster::{ClusterLink, StreamMessage};
use crate::db::engine::DatabaseEngine;
use crate::db::replication::wait_for_replay;
use crate::model::enums::SekaiServerRegion;

/// Silence on the socket longer than this (the writer pings every
/// `ping_interval_secs`, 15 s by default) is treated as a dead link.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct SubscriberConfig {
    pub writer_url: String,
    pub token: String,
    pub replica_wait: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

#[derive(Clone)]
pub struct SubscriberDeps {
    pub dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    pub api_cache_redis: Option<ConnectionManager>,
    pub realtime: RealtimeHub,
    pub link: Arc<ClusterLink>,
}

#[derive(Debug, thiserror::Error)]
pub enum SubscriberError {
    #[error("writer_url must be an http:// or https:// URL, got `{0}`")]
    BadUrl(String),
    #[error("writer_url uses https but the update stream client is built without TLS")]
    TlsUnsupported,
}

/// Translate the configured writer base URL into the WebSocket endpoint.
pub fn stream_url(writer_url: &str) -> Result<String, SubscriberError> {
    let trimmed = writer_url.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("http://") {
        Ok(format!("ws://{rest}/internal/updates"))
    } else if trimmed.starts_with("https://") {
        Err(SubscriberError::TlsUnsupported)
    } else {
        Err(SubscriberError::BadUrl(writer_url.to_owned()))
    }
}

/// Runs until the process exits. Spawn it once per reader.
pub async fn run(cfg: SubscriberConfig, deps: SubscriberDeps) {
    let url = match stream_url(&cfg.writer_url) {
        Ok(url) => url,
        Err(err) => {
            tracing::error!(%err, "cluster subscriber disabled");
            return;
        }
    };
    let mut applier = Applier::new(deps);
    let mut backoff = cfg.reconnect_min.max(Duration::from_millis(100));
    let mut first = true;
    loop {
        if !first {
            applier.deps.link.record_reconnect();
        }
        first = false;
        match connect_and_pump(&url, &cfg, &mut applier).await {
            Ok(()) => tracing::warn!("writer update stream closed"),
            Err(err) => tracing::warn!(%err, "writer update stream failed"),
        }
        applier.deps.link.set_connected(false);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(cfg.reconnect_max.max(backoff));
        if applier.was_healthy {
            // A link that had been streaming fine gets a fast first retry.
            backoff = cfg.reconnect_min.max(Duration::from_millis(100));
            applier.was_healthy = false;
        }
    }
}

async fn connect_and_pump(
    url: &str,
    cfg: &SubscriberConfig,
    applier: &mut Applier,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {}", cfg.token).parse()?);
    let (stream, _) = tokio_tungstenite::connect_async(request).await?;
    let (mut sink, mut source) = stream.split();
    tracing::info!(url, "connected to writer update stream");
    applier.deps.link.set_connected(true);

    loop {
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, source.next()).await {
            Ok(Some(frame)) => frame?,
            Ok(None) => return Ok(()),
            Err(_) => return Err("no frame within idle timeout".into()),
        };
        match frame {
            Message::Text(text) => {
                let message: StreamMessage = match sonic_rs::from_str(&text) {
                    Ok(message) => message,
                    Err(err) => {
                        tracing::warn!(%err, "unparseable update stream frame");
                        continue;
                    }
                };
                applier.apply(message, cfg.replica_wait).await;
            }
            Message::Ping(payload) => sink.send(Message::Pong(payload)).await?,
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

struct Applier {
    deps: SubscriberDeps,
    /// Topics this process has seen; the resync target after a gap.
    seen: HashSet<(SekaiServerRegion, i64)>,
    expected_seq: Option<u64>,
    was_healthy: bool,
}

impl Applier {
    fn new(deps: SubscriberDeps) -> Self {
        Self {
            deps,
            seen: HashSet::new(),
            expected_seq: None,
            was_healthy: false,
        }
    }

    async fn apply(&mut self, message: StreamMessage, replica_wait: Duration) {
        match message {
            StreamMessage::Hello { seq } => {
                // A hello mid-stream (writer restart, or it detected our
                // receiver lagging) means events may have been missed.
                let missed = self
                    .expected_seq
                    .is_some_and(|expected| expected != seq + 1);
                if missed || self.deps.link.last_seq() != 0 {
                    self.resync().await;
                }
                self.expected_seq = Some(seq + 1);
                self.was_healthy = true;
            }
            StreamMessage::Updated {
                seq,
                server,
                event_id,
                timestamp,
                lsn,
            } => {
                if let Some(expected) = self.expected_seq
                    && seq > expected
                {
                    tracing::warn!(expected, seq, "update stream gap; resyncing");
                    self.resync().await;
                }
                self.expected_seq = Some(seq + 1);
                self.seen.insert((server, event_id));
                if let Some(lsn) = lsn.as_deref()
                    && !replica_wait.is_zero()
                    && let Some(engine) = self.deps.dbs.get(&server)
                    && !wait_for_replay(engine, lsn, replica_wait).await
                {
                    tracing::warn!(%server, event_id, lsn, "replica did not reach lsn in time; invalidating anyway");
                }
                self.invalidate(server, event_id, timestamp).await;
                self.deps.link.record_update(seq, timestamp);
                self.was_healthy = true;
            }
            StreamMessage::Ping => {}
        }
    }

    async fn resync(&mut self) {
        let now = chrono::Utc::now().timestamp();
        let topics: Vec<_> = self.seen.iter().copied().collect();
        tracing::info!(
            topics = topics.len(),
            "resyncing caches for every known event"
        );
        for (server, event_id) in topics {
            self.invalidate(server, event_id, now).await;
        }
    }

    async fn invalidate(&mut self, server: SekaiServerRegion, event_id: i64, timestamp: i64) {
        // The push carries the post-bump epoch, so a client fetching with
        // `v=<version>` asks for exactly the data this update produced.
        let version = match self.deps.api_cache_redis.as_mut() {
            Some(conn) => match finish_event_update(conn, server, event_id).await {
                Ok(epoch) => Some(epoch),
                Err(err) => {
                    tracing::warn!(%err, %server, event_id, "failed to bump API cache epoch");
                    None
                }
            },
            None => None,
        };
        self.deps
            .realtime
            .notify_update(RealtimeTopic::new(server, event_id), timestamp, version);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps() -> (SubscriberDeps, RealtimeHub) {
        let realtime = RealtimeHub::new();
        (
            SubscriberDeps {
                dbs: HashMap::new(),
                api_cache_redis: None,
                realtime: realtime.clone(),
                link: Arc::new(ClusterLink::default()),
            },
            realtime,
        )
    }

    #[test]
    fn stream_url_maps_http_only() {
        assert_eq!(
            stream_url("http://writer:8777/").unwrap(),
            "ws://writer:8777/internal/updates"
        );
        assert!(matches!(
            stream_url("https://writer"),
            Err(SubscriberError::TlsUnsupported)
        ));
        assert!(matches!(
            stream_url("writer:8777"),
            Err(SubscriberError::BadUrl(_))
        ));
    }

    #[tokio::test]
    async fn updates_notify_realtime_and_gaps_resync_seen_topics() {
        let (deps, realtime) = deps();
        let mut rx = realtime.subscribe();
        let mut applier = Applier::new(deps);
        applier
            .apply(StreamMessage::Hello { seq: 0 }, Duration::ZERO)
            .await;
        applier
            .apply(
                StreamMessage::Updated {
                    seq: 1,
                    server: SekaiServerRegion::Cn,
                    event_id: 179,
                    timestamp: 10,
                    lsn: Some("0/1".into()),
                },
                Duration::ZERO,
            )
            .await;
        let first = rx.try_recv().unwrap();
        assert!(matches!(
            first,
            crate::api::realtime::RealtimeMessage::Updated { ref topic, timestamp: 10, version: None }
                if topic.event_id == 179
        ));
        assert_eq!(applier.deps.link.last_seq(), 1);

        // seq 2 was missed: the gap re-notifies the known topic, then the
        // event itself is applied.
        applier
            .apply(
                StreamMessage::Updated {
                    seq: 3,
                    server: SekaiServerRegion::Cn,
                    event_id: 179,
                    timestamp: 30,
                    lsn: None,
                },
                Duration::ZERO,
            )
            .await;
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert_eq!(applier.expected_seq, Some(4));

        // A fresh hello after streaming resyncs too.
        applier
            .apply(StreamMessage::Hello { seq: 0 }, Duration::ZERO)
            .await;
        assert!(rx.try_recv().is_ok());
        applier.apply(StreamMessage::Ping, Duration::ZERO).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn updates_carry_the_post_bump_cache_epoch() {
        let Ok(url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
            return;
        };
        let conn = redis::aio::ConnectionManager::new(redis::Client::open(url).unwrap())
            .await
            .unwrap();
        let (mut deps, realtime) = deps();
        deps.api_cache_redis = Some(conn);
        let mut rx = realtime.subscribe();
        let mut applier = Applier::new(deps);
        let event_id = chrono::Utc::now().timestamp_micros();
        for (seq, expected) in [(1, 1), (2, 2)] {
            applier
                .apply(
                    StreamMessage::Updated {
                        seq,
                        server: SekaiServerRegion::Cn,
                        event_id,
                        timestamp: 10,
                        lsn: None,
                    },
                    Duration::ZERO,
                )
                .await;
            let crate::api::realtime::RealtimeMessage::Updated { version, .. } =
                rx.try_recv().unwrap()
            else {
                panic!("expected update");
            };
            assert_eq!(version, Some(expected));
        }
    }
}
