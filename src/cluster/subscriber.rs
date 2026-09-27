//! Reader side of the update stream: dial the writer, apply every
//! `updated` event to the local cache epoch and realtime hub, reconnect
//! forever with bounded backoff.
//!
//! Frames that have already arrived are applied as one batch: the events
//! are coalesced per `(server, event_id)` and the replica is waited for
//! once, on the highest WAL position of the batch. Replay is instance-wide,
//! so a replica that has caught up with the newest position holds every
//! older one too; a stalled replica costs one `replica_wait` per batch
//! instead of one per message.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt, SinkExt, StreamExt};
use redis::aio::ConnectionManager;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::api::cache::finish_event_update;
use crate::api::realtime::{RealtimeHub, RealtimeTopic};
use crate::cluster::{ClusterLink, StreamMessage};
use crate::db::engine::DatabaseEngine;
use crate::db::replication::replay_reached;
use crate::model::enums::SekaiServerRegion;

/// Silence on the socket longer than this (the writer pings every
/// `ping_interval_secs`, 15 s by default) is treated as a dead link.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

const REPLAY_POLL: Duration = Duration::from_millis(25);

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
    let backend = LiveBackend {
        dbs: deps.dbs.clone(),
        api_cache_redis: deps.api_cache_redis.clone(),
    };
    let mut applier = Applier::new(deps, backend);
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

type PumpError = Box<dyn std::error::Error + Send + Sync>;

async fn connect_and_pump(
    url: &str,
    cfg: &SubscriberConfig,
    applier: &mut Applier<LiveBackend>,
) -> Result<(), PumpError> {
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
        let mut batch = Vec::new();
        let mut open = ingest(frame, &mut sink, &mut batch).await?;
        // Take whatever else has already arrived (a burst behind a slow
        // replica wait, or the backlog after a reconnect) so it is applied
        // as one batch.
        while open {
            match source.next().now_or_never() {
                None => break,
                Some(None) => open = false,
                Some(Some(frame)) => open = ingest(frame?, &mut sink, &mut batch).await?,
            }
        }
        applier.apply_batch(batch, cfg.replica_wait).await;
        if !open {
            return Ok(());
        }
    }
}

/// Parses one frame into `batch`, answering pings inline. Returns whether
/// the stream is still open.
async fn ingest<S>(
    frame: Message,
    sink: &mut S,
    batch: &mut Vec<StreamMessage>,
) -> Result<bool, PumpError>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    match frame {
        Message::Text(text) => match sonic_rs::from_str::<StreamMessage>(&text) {
            Ok(message) => batch.push(message),
            Err(err) => tracing::warn!(%err, "unparseable update stream frame"),
        },
        Message::Ping(payload) => sink.send(Message::Pong(payload)).await?,
        Message::Close(_) => return Ok(false),
        _ => {}
    }
    Ok(true)
}

/// What the applier needs from the outside world, abstracted so batching
/// can be tested against a fake replica and cache.
pub trait ApplierBackend {
    /// Whether this reader's database for `server` has replayed `lsn`.
    /// `None` when there is no database for the region.
    fn replay_reached(
        &self,
        server: SekaiServerRegion,
        lsn: &str,
    ) -> impl Future<Output = Option<Result<bool, String>>> + Send;

    /// Bumps the event's API cache epoch, returning the new value; `None`
    /// when this process has no API cache.
    fn bump_epoch(
        &mut self,
        server: SekaiServerRegion,
        event_id: i64,
    ) -> impl Future<Output = Option<Result<i64, String>>> + Send;
}

struct LiveBackend {
    dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>,
    api_cache_redis: Option<ConnectionManager>,
}

impl ApplierBackend for LiveBackend {
    async fn replay_reached(
        &self,
        server: SekaiServerRegion,
        lsn: &str,
    ) -> Option<Result<bool, String>> {
        let engine = self.dbs.get(&server)?;
        Some(
            replay_reached(engine, lsn)
                .await
                .map_err(|err| err.to_string()),
        )
    }

    async fn bump_epoch(
        &mut self,
        server: SekaiServerRegion,
        event_id: i64,
    ) -> Option<Result<i64, String>> {
        let conn = self.api_cache_redis.as_mut()?;
        Some(
            finish_event_update(conn, server, event_id)
                .await
                .map_err(|err| err.to_string()),
        )
    }
}

/// Postgres `X/Y` WAL position as a comparable integer.
fn parse_lsn(lsn: &str) -> Option<u64> {
    let (hi, lo) = lsn.split_once('/')?;
    let hi = u64::from_str_radix(hi, 16).ok()?;
    let lo = u64::from_str_radix(lo, 16).ok()?;
    (hi <= u32::MAX as u64 && lo <= u32::MAX as u64).then_some((hi << 32) | lo)
}

#[derive(Debug, Default)]
struct PendingKey {
    timestamp: i64,
    /// Highest position announced for this key.
    lsn: Option<(u64, String)>,
    /// Some update for this key carried no position, so the batch cannot
    /// vouch for the key even if the replica caught up.
    unvouched: bool,
}

/// Updates received in one batch, coalesced per topic in first-seen order.
#[derive(Debug, Default)]
struct Pending {
    keys: Vec<((SekaiServerRegion, i64), PendingKey)>,
    messages: usize,
    last: Option<(u64, i64)>,
}

impl Pending {
    fn push(
        &mut self,
        seq: u64,
        server: SekaiServerRegion,
        event_id: i64,
        timestamp: i64,
        lsn: Option<String>,
    ) {
        self.messages += 1;
        self.last = Some((seq, timestamp));
        let key = (server, event_id);
        let entry = match self.keys.iter_mut().find(|(k, _)| *k == key) {
            Some((_, entry)) => entry,
            None => {
                self.keys.push((key, PendingKey::default()));
                &mut self.keys.last_mut().expect("just pushed").1
            }
        };
        entry.timestamp = entry.timestamp.max(timestamp);
        match lsn.as_deref().and_then(parse_lsn) {
            Some(n) => {
                if entry.lsn.as_ref().is_none_or(|(cur, _)| n > *cur) {
                    entry.lsn = Some((n, lsn.expect("parsed")));
                }
            }
            None => entry.unvouched = true,
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Highest position per server, highest overall first.
    fn max_lsns(&self) -> Vec<(SekaiServerRegion, u64, String)> {
        let mut out: Vec<(SekaiServerRegion, u64, String)> = Vec::new();
        for ((server, _), key) in &self.keys {
            let Some((n, lsn)) = &key.lsn else { continue };
            match out.iter_mut().find(|(s, ..)| s == server) {
                Some(cur) if cur.1 >= *n => {}
                Some(cur) => *cur = (*server, *n, lsn.clone()),
                None => out.push((*server, *n, lsn.clone())),
            }
        }
        out.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        out
    }
}

struct Applier<B> {
    deps: SubscriberDeps,
    backend: B,
    /// Topics this process has seen; the resync target after a gap.
    seen: HashSet<(SekaiServerRegion, i64)>,
    expected_seq: Option<u64>,
    was_healthy: bool,
}

impl<B: ApplierBackend> Applier<B> {
    fn new(deps: SubscriberDeps, backend: B) -> Self {
        Self {
            deps,
            backend,
            seen: HashSet::new(),
            expected_seq: None,
            was_healthy: false,
        }
    }

    /// Applies the messages in stream order. Updates are coalesced until a
    /// `hello` or a sequence gap forces them out (those resync, and the
    /// resync must observe everything before it), then once at the end.
    async fn apply_batch(&mut self, messages: Vec<StreamMessage>, replica_wait: Duration) {
        let mut pending = Pending::default();
        for message in messages {
            match message {
                StreamMessage::Hello { seq } => {
                    self.flush(&mut pending, replica_wait).await;
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
                        self.flush(&mut pending, replica_wait).await;
                        tracing::warn!(expected, seq, "update stream gap; resyncing");
                        self.resync().await;
                    }
                    self.expected_seq = Some(seq + 1);
                    self.seen.insert((server, event_id));
                    pending.push(seq, server, event_id, timestamp, lsn);
                    self.was_healthy = true;
                }
                StreamMessage::Ping => {}
            }
        }
        self.flush(&mut pending, replica_wait).await;
    }

    async fn flush(&mut self, pending: &mut Pending, replica_wait: Duration) {
        if pending.is_empty() {
            return;
        }
        let pending = std::mem::take(pending);
        let reached = self.wait_for_batch(&pending, replica_wait).await;
        for ((server, event_id), key) in &pending.keys {
            let replayed = key.lsn.is_some()
                && !key.unvouched
                && reached.get(server).copied().unwrap_or(false);
            self.invalidate(*server, *event_id, key.timestamp, replayed)
                .await;
        }
        if let Some((seq, timestamp)) = pending.last {
            self.deps.link.record_update(seq, timestamp);
        }
    }

    /// One bounded wait on the batch's highest position; every other
    /// server's highest position is then probed once without waiting
    /// (with a shared instance it is already covered, with separate
    /// instances the probe still answers correctly).
    async fn wait_for_batch(
        &self,
        pending: &Pending,
        replica_wait: Duration,
    ) -> HashMap<SekaiServerRegion, bool> {
        let mut reached = HashMap::new();
        let mut lagging = Vec::new();
        let mut budget = replica_wait;
        for (server, _, lsn) in pending.max_lsns() {
            let ok = self.wait_for(server, &lsn, budget).await;
            budget = Duration::ZERO;
            reached.insert(server, ok);
            if !ok {
                lagging.push(format!("{server}@{lsn}"));
            }
        }
        if !lagging.is_empty() {
            tracing::warn!(
                lagging = %lagging.join(","),
                messages = pending.messages,
                topics = pending.keys.len(),
                "replica did not reach lsn in time; invalidating anyway"
            );
        }
        reached
    }

    /// Whether this reader's database is known to hold the update: the
    /// writer sent its WAL position and the local engine replayed it (a
    /// primary trivially has). With a zero budget the position is probed
    /// once without waiting. DB errors count as "not reached" (logged) so a
    /// flaky probe can only delay an invalidation, never skip it.
    async fn wait_for(&self, server: SekaiServerRegion, lsn: &str, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match self.backend.replay_reached(server, lsn).await {
                None => return false,
                Some(Ok(true)) => return true,
                Some(Ok(false)) => {}
                Some(Err(err)) => {
                    tracing::warn!(%err, %server, lsn, "replay probe failed");
                    return false;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(REPLAY_POLL).await;
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
            self.invalidate(server, event_id, now, false).await;
        }
    }

    /// Bumps the event's cache epoch and pushes `updated`. The push carries
    /// the new epoch as `version` only when `replayed`: a client fetches
    /// `v=<version>` as immutable, so a version is announced only once this
    /// reader's database is known to hold the data behind it. Otherwise the
    /// epoch still moves (caches refetch) but clients get no version and
    /// stay on short-lived responses until the next confirmed update.
    async fn invalidate(
        &mut self,
        server: SekaiServerRegion,
        event_id: i64,
        timestamp: i64,
        replayed: bool,
    ) {
        let version = match self.backend.bump_epoch(server, event_id).await {
            Some(Ok(epoch)) => replayed.then_some(epoch),
            Some(Err(err)) => {
                tracing::warn!(%err, %server, event_id, "failed to bump API cache epoch");
                None
            }
            None => None,
        };
        self.deps
            .realtime
            .notify_update(RealtimeTopic::new(server, event_id), timestamp, version);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::sync::broadcast::Receiver;

    use super::*;
    use crate::api::realtime::RealtimeMessage;

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

    /// A replica that is stalled until `caught_up` flips, plus an in-memory
    /// epoch counter per topic.
    #[derive(Default)]
    struct FakeBackend {
        caught_up: Arc<AtomicBool>,
        probes: Arc<AtomicUsize>,
        epochs: Arc<Mutex<HashMap<(SekaiServerRegion, i64), i64>>>,
    }

    impl ApplierBackend for FakeBackend {
        async fn replay_reached(
            &self,
            _server: SekaiServerRegion,
            _lsn: &str,
        ) -> Option<Result<bool, String>> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            Some(Ok(self.caught_up.load(Ordering::SeqCst)))
        }

        async fn bump_epoch(
            &mut self,
            server: SekaiServerRegion,
            event_id: i64,
        ) -> Option<Result<i64, String>> {
            let mut epochs = self.epochs.lock().unwrap();
            let epoch = epochs.entry((server, event_id)).or_insert(0);
            *epoch += 1;
            Some(Ok(*epoch))
        }
    }

    fn updated(seq: u64, server: SekaiServerRegion, event_id: i64, lsn: &str) -> StreamMessage {
        StreamMessage::Updated {
            seq,
            server,
            event_id,
            timestamp: 1_000 + seq as i64,
            lsn: Some(lsn.into()),
        }
    }

    fn drain(
        rx: &mut Receiver<RealtimeMessage>,
    ) -> Vec<(SekaiServerRegion, i64, i64, Option<i64>)> {
        let mut out = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let RealtimeMessage::Updated {
                topic,
                timestamp,
                version,
            } = message
            {
                out.push((topic.server, topic.event_id, timestamp, version));
            }
        }
        out
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

    #[test]
    fn lsn_parses_and_orders() {
        assert_eq!(parse_lsn("0/1"), Some(1));
        assert_eq!(parse_lsn("1/0"), Some(1 << 32));
        assert!(parse_lsn("A/FF") > parse_lsn("9/FFFFFFFF"));
        assert_eq!(parse_lsn("nope"), None);
        assert_eq!(parse_lsn("1/100000000"), None);
    }

    #[tokio::test]
    async fn updates_notify_realtime_and_gaps_resync_seen_topics() {
        let (deps, realtime) = deps();
        let mut rx = realtime.subscribe();
        let mut applier = Applier::new(deps, FakeBackend::default());
        applier
            .apply_batch(vec![StreamMessage::Hello { seq: 0 }], Duration::ZERO)
            .await;
        applier
            .apply_batch(
                vec![StreamMessage::Updated {
                    seq: 1,
                    server: SekaiServerRegion::Cn,
                    event_id: 179,
                    timestamp: 10,
                    lsn: Some("0/1".into()),
                }],
                Duration::ZERO,
            )
            .await;
        let first = rx.try_recv().unwrap();
        // The fake replica is stalled: the epoch moved but no version is
        // vouched for.
        assert!(matches!(
            first,
            RealtimeMessage::Updated { ref topic, timestamp: 10, version: None }
                if topic.event_id == 179
        ));
        assert_eq!(applier.deps.link.last_seq(), 1);

        // seq 2 was missed: the gap re-notifies the known topic, then the
        // event itself is applied.
        applier
            .apply_batch(
                vec![StreamMessage::Updated {
                    seq: 3,
                    server: SekaiServerRegion::Cn,
                    event_id: 179,
                    timestamp: 30,
                    lsn: None,
                }],
                Duration::ZERO,
            )
            .await;
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert_eq!(applier.expected_seq, Some(4));

        // A fresh hello after streaming resyncs too.
        applier
            .apply_batch(vec![StreamMessage::Hello { seq: 0 }], Duration::ZERO)
            .await;
        assert!(rx.try_recv().is_ok());
        applier
            .apply_batch(vec![StreamMessage::Ping], Duration::ZERO)
            .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_behind_a_stalled_replica_costs_one_wait() {
        let (deps, realtime) = deps();
        let mut rx = realtime.subscribe();
        let backend = FakeBackend::default();
        let probes = backend.probes.clone();
        let caught_up = backend.caught_up.clone();
        let epochs = backend.epochs.clone();
        let mut applier = Applier::new(deps, backend);
        let wait = Duration::from_millis(1500);

        // 30 updates alternating between two topics, positions increasing.
        let mut burst = vec![StreamMessage::Hello { seq: 0 }];
        for seq in 1..=30 {
            let (server, event_id) = if seq % 2 == 1 {
                (SekaiServerRegion::Cn, 180)
            } else {
                (SekaiServerRegion::Jp, 218)
            };
            burst.push(updated(seq, server, event_id, &format!("0/{seq:X}")));
        }
        let started = tokio::time::Instant::now();
        applier.apply_batch(burst, wait).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= wait && elapsed < wait * 2,
            "one bounded wait, got {elapsed:?}"
        );
        // One polling loop on the top position plus a single probe for
        // the other server, not 30 waits.
        assert!(probes.load(Ordering::SeqCst) <= 1500 / 25 + 2);
        // Each topic invalidated once, with its latest timestamp and no
        // version (nothing was vouched for).
        assert_eq!(
            drain(&mut rx),
            vec![
                (SekaiServerRegion::Cn, 180, 1_029, None),
                (SekaiServerRegion::Jp, 218, 1_030, None),
            ]
        );
        assert_eq!(epochs.lock().unwrap()[&(SekaiServerRegion::Cn, 180)], 1);
        assert_eq!(epochs.lock().unwrap()[&(SekaiServerRegion::Jp, 218)], 1);
        assert_eq!(applier.deps.link.last_seq(), 30);
        assert_eq!(applier.expected_seq, Some(31));

        // Once the replica is current the next batch vouches for both
        // topics with their own post-bump epochs, without polling.
        caught_up.store(true, Ordering::SeqCst);
        probes.store(0, Ordering::SeqCst);
        let started = tokio::time::Instant::now();
        applier
            .apply_batch(
                vec![
                    updated(31, SekaiServerRegion::Jp, 218, "0/20"),
                    updated(32, SekaiServerRegion::Cn, 180, "0/21"),
                    updated(33, SekaiServerRegion::Cn, 180, "0/22"),
                ],
                wait,
            )
            .await;
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        assert_eq!(
            drain(&mut rx),
            vec![
                (SekaiServerRegion::Jp, 218, 1_031, Some(2)),
                (SekaiServerRegion::Cn, 180, 1_033, Some(2)),
            ]
        );
        assert_eq!(applier.deps.link.last_seq(), 33);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hello_or_gap_flushes_what_came_before_it() {
        let (deps, realtime) = deps();
        let mut rx = realtime.subscribe();
        let backend = FakeBackend::default();
        backend.caught_up.store(true, Ordering::SeqCst);
        let mut applier = Applier::new(deps, backend);
        applier
            .apply_batch(
                vec![
                    StreamMessage::Hello { seq: 0 },
                    updated(1, SekaiServerRegion::Cn, 180, "0/1"),
                    // Writer restarted: its hello must see seq 1 applied
                    // first, then resync the topic.
                    StreamMessage::Hello { seq: 0 },
                    updated(1, SekaiServerRegion::Jp, 218, "0/1"),
                    // seq 2 skipped: flush jp, resync both, then apply.
                    updated(3, SekaiServerRegion::Jp, 218, "0/3"),
                ],
                Duration::from_millis(100),
            )
            .await;
        let pushes = drain(&mut rx);
        assert_eq!(pushes[0], (SekaiServerRegion::Cn, 180, 1_001, Some(1)));
        // Resync after the hello: cn/180 again, unversioned.
        assert_eq!(pushes[1].1, 180);
        assert_eq!(pushes[1].3, None);
        assert_eq!(pushes[2], (SekaiServerRegion::Jp, 218, 1_001, Some(1)));
        // Gap resync covers both known topics (order is set-dependent).
        let mut resynced: Vec<_> = pushes[3..5].iter().map(|p| p.1).collect();
        resynced.sort_unstable();
        assert_eq!(resynced, vec![180, 218]);
        assert!(pushes[3..5].iter().all(|p| p.3.is_none()));
        assert_eq!(pushes[5], (SekaiServerRegion::Jp, 218, 1_003, Some(3)));
        assert_eq!(pushes.len(), 6);
        assert_eq!(applier.expected_seq, Some(4));
        assert_eq!(applier.deps.link.last_seq(), 3);
    }

    #[tokio::test]
    async fn a_positionless_update_is_never_vouched_for() {
        let (deps, realtime) = deps();
        let mut rx = realtime.subscribe();
        let backend = FakeBackend::default();
        backend.caught_up.store(true, Ordering::SeqCst);
        let mut applier = Applier::new(deps, backend);
        applier
            .apply_batch(
                vec![
                    updated(1, SekaiServerRegion::Cn, 180, "0/1"),
                    StreamMessage::Updated {
                        seq: 2,
                        server: SekaiServerRegion::Cn,
                        event_id: 180,
                        timestamp: 5,
                        lsn: None,
                    },
                    updated(3, SekaiServerRegion::Cn, 181, "0/3"),
                ],
                Duration::ZERO,
            )
            .await;
        assert_eq!(
            drain(&mut rx),
            vec![
                (SekaiServerRegion::Cn, 180, 1_001, None),
                (SekaiServerRegion::Cn, 181, 1_003, Some(1)),
            ]
        );
    }

    #[tokio::test]
    async fn updates_carry_a_version_only_once_the_data_is_replayed() {
        let Ok(url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
            return;
        };
        let conn = redis::aio::ConnectionManager::new(redis::Client::open(url).unwrap())
            .await
            .unwrap();
        let (deps, realtime) = deps();
        // SQLite has no replication: any position counts as replayed.
        let engine = crate::db::engine::DatabaseEngine::from_connection(
            sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
            sea_orm::DatabaseBackend::Sqlite,
        );
        let backend = LiveBackend {
            dbs: HashMap::from([(SekaiServerRegion::Cn, Arc::new(engine))]),
            api_cache_redis: Some(conn),
        };
        let mut rx = realtime.subscribe();
        let mut applier = Applier::new(deps, backend);
        let event_id = chrono::Utc::now().timestamp_micros();
        let next = |seq: u64, lsn: Option<&str>| StreamMessage::Updated {
            seq,
            server: SekaiServerRegion::Cn,
            event_id,
            timestamp: 10,
            lsn: lsn.map(str::to_owned),
        };
        let version = |rx: &mut Receiver<RealtimeMessage>| {
            let RealtimeMessage::Updated { version, .. } = rx.try_recv().unwrap() else {
                panic!("expected update");
            };
            version
        };
        // Replayed (with and without a wait budget): the post-bump epoch.
        applier
            .apply_batch(vec![next(1, Some("0/1"))], Duration::from_millis(50))
            .await;
        assert_eq!(version(&mut rx), Some(1));
        applier
            .apply_batch(vec![next(2, Some("0/2"))], Duration::ZERO)
            .await;
        assert_eq!(version(&mut rx), Some(2));
        // No WAL position: the epoch still moves, but nothing is vouched for.
        applier
            .apply_batch(vec![next(3, None)], Duration::ZERO)
            .await;
        assert_eq!(version(&mut rx), None);
        applier
            .apply_batch(vec![next(4, Some("0/4"))], Duration::ZERO)
            .await;
        assert_eq!(version(&mut rx), Some(4));
    }
}
