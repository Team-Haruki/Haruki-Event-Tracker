use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};
use tokio::time::Instant;

use crate::config::RealtimeConfig;
use crate::model::enums::SekaiServerRegion;

/// Realtime tuning shared by the hub and every socket it serves.
#[derive(Debug, Clone, Copy)]
pub struct RealtimeSettings {
    /// Minimum spacing between `updated` pushes per topic; zero pushes
    /// every update.
    pub push_min_interval: Duration,
    /// `online` counts are broadcast at most once per this interval per
    /// topic; zero broadcasts on every change.
    pub online_broadcast_interval: Duration,
    /// Server-side ping cadence; zero disables the keepalive.
    pub ws_ping_interval: Duration,
    /// A socket silent (no frame, pong included) for this long is closed.
    pub ws_idle_timeout: Duration,
}

impl Default for RealtimeSettings {
    fn default() -> Self {
        Self::from(&RealtimeConfig::default())
    }
}

impl RealtimeSettings {
    /// No throttling at all: every update and count change is broadcast
    /// as it happens (tests and the pre-throttle behaviour).
    pub fn immediate() -> Self {
        Self {
            push_min_interval: Duration::ZERO,
            online_broadcast_interval: Duration::ZERO,
            ..Self::default()
        }
    }
}

impl From<&RealtimeConfig> for RealtimeSettings {
    fn from(config: &RealtimeConfig) -> Self {
        Self {
            push_min_interval: Duration::from_secs(config.push_min_interval_secs),
            online_broadcast_interval: Duration::from_secs(config.online_broadcast_interval_secs),
            ws_ping_interval: Duration::from_secs(config.ws_ping_interval_secs),
            ws_idle_timeout: Duration::from_secs(config.ws_idle_timeout_secs),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeTopic {
    pub server: SekaiServerRegion,
    pub event_id: i64,
}

impl RealtimeTopic {
    pub fn new(server: SekaiServerRegion, event_id: i64) -> Self {
        Self { server, event_id }
    }
}

#[derive(Debug, Clone)]
pub enum RealtimeMessage {
    Updated {
        topic: RealtimeTopic,
        timestamp: i64,
        /// The event's API-cache epoch after the bump that triggered this
        /// push (`None` when this process has no API cache).
        version: Option<i64>,
    },
    Online {
        topic: RealtimeTopic,
        total: usize,
        topic_online: usize,
    },
}

#[derive(Clone)]
pub struct RealtimeHub {
    inner: Arc<Inner>,
}

struct Inner {
    tx: broadcast::Sender<RealtimeMessage>,
    online_total: AtomicUsize,
    online_by_topic: Mutex<HashMap<RealtimeTopic, usize>>,
    settings: RealtimeSettings,
    push_throttle: StdMutex<HashMap<RealtimeTopic, TopicThrottle>>,
    /// Topics whose `online` count changed since the last broadcast, and
    /// whether a delayed flush of them is already scheduled.
    online_dirty: StdMutex<HashSet<RealtimeTopic>>,
    online_flush_scheduled: AtomicBool,
}

/// Per-topic `updated` push throttle state. Suppressed updates coalesce
/// into `pending` (latest timestamp and version win) and one trailing task delivers
/// it when the window closes, so subscribers never miss the last change
/// of a burst — they just see it at most once per interval.
struct TopicThrottle {
    last_sent_at: Instant,
    pending: Option<(i64, Option<i64>)>,
    trailing_scheduled: bool,
}

impl Default for RealtimeHub {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeHub {
    /// A hub that broadcasts every change as it happens.
    pub fn new() -> Self {
        Self::with_settings(RealtimeSettings::immediate())
    }

    /// `min_push_interval` caps how often an `updated` event is pushed per
    /// topic; `Duration::ZERO` pushes every update.
    pub fn with_min_push_interval(min_push_interval: Duration) -> Self {
        Self::with_settings(RealtimeSettings {
            push_min_interval: min_push_interval,
            ..RealtimeSettings::immediate()
        })
    }

    pub fn with_settings(settings: RealtimeSettings) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                tx,
                online_total: AtomicUsize::new(0),
                online_by_topic: Mutex::new(HashMap::new()),
                settings,
                push_throttle: StdMutex::new(HashMap::new()),
                online_dirty: StdMutex::new(HashSet::new()),
                online_flush_scheduled: AtomicBool::new(false),
            }),
        }
    }

    pub fn settings(&self) -> &RealtimeSettings {
        &self.inner.settings
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RealtimeMessage> {
        self.inner.tx.subscribe()
    }

    pub fn connection_opened(&self) -> usize {
        self.inner.online_total.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub async fn connection_closed(&self, topics: &[RealtimeTopic]) {
        self.inner.online_total.fetch_sub(1, Ordering::Relaxed);
        for topic in topics {
            self.remove_topic_subscription(topic).await;
        }
    }

    pub async fn add_topic_subscription(&self, topic: RealtimeTopic) -> usize {
        let topic_online = {
            let mut online = self.inner.online_by_topic.lock().await;
            let count = online.entry(topic.clone()).or_insert(0);
            *count += 1;
            *count
        };
        self.broadcast_online(topic, topic_online);
        topic_online
    }

    pub async fn remove_topic_subscription(&self, topic: &RealtimeTopic) {
        let topic_online = {
            let mut online = self.inner.online_by_topic.lock().await;
            let Some(count) = online.get_mut(topic) else {
                return;
            };
            *count = count.saturating_sub(1);
            let next = *count;
            if next == 0 {
                online.remove(topic);
            }
            next
        };
        self.broadcast_online(topic.clone(), topic_online);
    }

    pub fn total_online(&self) -> usize {
        self.inner.online_total.load(Ordering::Relaxed)
    }

    pub async fn topic_online(&self, topic: &RealtimeTopic) -> usize {
        self.inner
            .online_by_topic
            .lock()
            .await
            .get(topic)
            .copied()
            .unwrap_or(0)
    }

    pub fn notify_update(&self, topic: RealtimeTopic, timestamp: i64, version: Option<i64>) {
        let interval = self.inner.settings.push_min_interval;
        if interval.is_zero() {
            let _ = self.inner.tx.send(RealtimeMessage::Updated {
                topic,
                timestamp,
                version,
            });
            return;
        }

        let now = Instant::now();
        let trailing_deadline = {
            let mut throttle = self
                .inner
                .push_throttle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match throttle.get_mut(&topic) {
                Some(state) if now.duration_since(state.last_sent_at) < interval => {
                    state.pending = Some((timestamp, version));
                    if state.trailing_scheduled {
                        return;
                    }
                    state.trailing_scheduled = true;
                    Some(state.last_sent_at + interval)
                }
                Some(state) => {
                    state.last_sent_at = now;
                    state.pending = None;
                    None
                }
                None => {
                    throttle.insert(
                        topic.clone(),
                        TopicThrottle {
                            last_sent_at: now,
                            pending: None,
                            trailing_scheduled: false,
                        },
                    );
                    None
                }
            }
        };

        match trailing_deadline {
            None => {
                let _ = self.inner.tx.send(RealtimeMessage::Updated {
                    topic,
                    timestamp,
                    version,
                });
            }
            Some(deadline) => {
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    tokio::time::sleep_until(deadline).await;
                    let pending = {
                        let mut throttle = inner
                            .push_throttle
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let Some(state) = throttle.get_mut(&topic) else {
                            return;
                        };
                        state.trailing_scheduled = false;
                        let pending = state.pending.take();
                        if pending.is_some() {
                            state.last_sent_at = Instant::now();
                        }
                        pending
                    };
                    if let Some((timestamp, version)) = pending {
                        let _ = inner.tx.send(RealtimeMessage::Updated {
                            topic,
                            timestamp,
                            version,
                        });
                    }
                });
            }
        }
    }

    /// Announces a topic's count: immediately when unthrottled, otherwise
    /// the topic is marked dirty and one delayed flush sends the counts as
    /// they stand when the interval elapses — a subscribe/unsubscribe
    /// storm costs subscribers one frame per topic per interval.
    fn broadcast_online(&self, topic: RealtimeTopic, topic_online: usize) {
        let interval = self.inner.settings.online_broadcast_interval;
        if interval.is_zero() {
            let _ = self.inner.tx.send(RealtimeMessage::Online {
                topic,
                total: self.total_online(),
                topic_online,
            });
            return;
        }
        self.inner
            .online_dirty
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(topic);
        if self
            .inner
            .online_flush_scheduled
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let inner = self.inner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(interval).await;
            // Clear the flag before draining so a change that lands during
            // the drain schedules the next flush instead of getting lost.
            inner.online_flush_scheduled.store(false, Ordering::Release);
            let dirty: Vec<RealtimeTopic> = inner
                .online_dirty
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain()
                .collect();
            let total = inner.online_total.load(Ordering::Relaxed);
            let online = inner.online_by_topic.lock().await;
            for topic in dirty {
                let topic_online = online.get(&topic).copied().unwrap_or(0);
                let _ = inner.tx.send(RealtimeMessage::Online {
                    topic,
                    total,
                    topic_online,
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_updated(msg: RealtimeMessage) -> (RealtimeTopic, i64) {
        expect_versioned(msg).0
    }

    fn expect_versioned(msg: RealtimeMessage) -> ((RealtimeTopic, i64), Option<i64>) {
        match msg {
            RealtimeMessage::Updated {
                topic,
                timestamp,
                version,
            } => ((topic, timestamp), version),
            RealtimeMessage::Online { .. } => panic!("expected update message"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn throttles_update_pushes_with_trailing_coalescing() {
        let hub = RealtimeHub::with_min_push_interval(Duration::from_secs(5));
        let topic = RealtimeTopic::new(SekaiServerRegion::Jp, 7);
        let mut receiver = hub.subscribe();

        // First push of a window goes out immediately.
        hub.notify_update(topic.clone(), 1, Some(11));
        assert_eq!(
            expect_versioned(receiver.recv().await.unwrap()),
            ((topic.clone(), 1), Some(11))
        );

        // Updates inside the window coalesce; the latest timestamp and
        // version win.
        hub.notify_update(topic.clone(), 2, Some(12));
        hub.notify_update(topic.clone(), 3, Some(13));
        assert!(receiver.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(
            expect_versioned(receiver.recv().await.unwrap()),
            ((topic.clone(), 3), Some(13))
        );

        // A quiet window resets to immediate delivery.
        tokio::time::advance(Duration::from_secs(6)).await;
        hub.notify_update(topic.clone(), 4, None);
        assert_eq!(expect_updated(receiver.recv().await.unwrap()).1, 4);
        // No stray trailing push follows.
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(receiver.try_recv().is_err());
    }

    fn expect_online(msg: RealtimeMessage) -> (RealtimeTopic, usize, usize) {
        match msg {
            RealtimeMessage::Online {
                topic,
                total,
                topic_online,
            } => (topic, total, topic_online),
            RealtimeMessage::Updated { .. } => panic!("expected online message"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn online_counts_are_broadcast_at_most_once_per_interval() {
        let hub = RealtimeHub::with_settings(RealtimeSettings {
            online_broadcast_interval: Duration::from_secs(2),
            ..RealtimeSettings::immediate()
        });
        let jp = RealtimeTopic::new(SekaiServerRegion::Jp, 1);
        let en = RealtimeTopic::new(SekaiServerRegion::En, 2);
        let mut receiver = hub.subscribe();

        // A storm of changes inside one window collapses into one frame
        // per touched topic, carrying the counts at flush time.
        hub.connection_opened();
        hub.connection_opened();
        hub.add_topic_subscription(jp.clone()).await;
        hub.add_topic_subscription(jp.clone()).await;
        hub.add_topic_subscription(en.clone()).await;
        hub.remove_topic_subscription(&jp).await;
        assert!(receiver.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(3)).await;
        let mut seen: Vec<_> = vec![
            expect_online(receiver.recv().await.unwrap()),
            expect_online(receiver.recv().await.unwrap()),
        ];
        seen.sort_by_key(|(topic, _, _)| topic.event_id);
        assert_eq!(seen, vec![(jp.clone(), 2, 1), (en.clone(), 2, 1)]);
        assert!(receiver.try_recv().is_err());

        // A change after the flush schedules a fresh one.
        hub.connection_closed(&[jp.clone(), en.clone()]).await;
        assert!(receiver.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(3)).await;
        let mut seen = vec![
            expect_online(receiver.recv().await.unwrap()),
            expect_online(receiver.recv().await.unwrap()),
        ];
        seen.sort_by_key(|(topic, _, _)| topic.event_id);
        assert_eq!(seen, vec![(jp, 1, 0), (en, 1, 0)]);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn settings_follow_the_config_and_immediate_disables_throttling() {
        let settings = RealtimeSettings::default();
        assert_eq!(settings.push_min_interval, Duration::ZERO);
        assert_eq!(settings.online_broadcast_interval, Duration::from_secs(2));
        assert_eq!(settings.ws_ping_interval, Duration::from_secs(30));
        assert_eq!(settings.ws_idle_timeout, Duration::from_secs(75));
        let immediate = RealtimeSettings::immediate();
        assert!(immediate.online_broadcast_interval.is_zero());
        assert_eq!(immediate.ws_ping_interval, settings.ws_ping_interval);
        assert!(
            RealtimeHub::new()
                .settings()
                .online_broadcast_interval
                .is_zero()
        );
    }

    #[tokio::test]
    async fn tracks_connections_topics_and_broadcasts_updates() {
        let hub = RealtimeHub::new();
        let topic = RealtimeTopic::new(SekaiServerRegion::En, 42);
        let mut receiver = hub.subscribe();

        assert_eq!(hub.connection_opened(), 1);
        assert_eq!(hub.total_online(), 1);
        assert_eq!(hub.add_topic_subscription(topic.clone()).await, 1);
        assert_eq!(hub.topic_online(&topic).await, 1);
        match receiver.recv().await.unwrap() {
            RealtimeMessage::Online {
                topic: received,
                total,
                topic_online,
            } => {
                assert_eq!(received, topic);
                assert_eq!(total, 1);
                assert_eq!(topic_online, 1);
            }
            RealtimeMessage::Updated { .. } => panic!("expected online message"),
        }

        hub.notify_update(topic.clone(), 1234, Some(7));
        match receiver.recv().await.unwrap() {
            RealtimeMessage::Updated {
                topic: received,
                timestamp,
                version,
            } => {
                assert_eq!(received, topic);
                assert_eq!(timestamp, 1234);
                assert_eq!(version, Some(7));
            }
            RealtimeMessage::Online { .. } => panic!("expected update message"),
        }

        hub.remove_topic_subscription(&topic).await;
        assert_eq!(hub.topic_online(&topic).await, 0);
        assert!(matches!(
            receiver.recv().await.unwrap(),
            RealtimeMessage::Online {
                topic_online: 0,
                ..
            }
        ));
        hub.remove_topic_subscription(&topic).await;

        assert_eq!(hub.add_topic_subscription(topic.clone()).await, 1);
        hub.connection_closed(std::slice::from_ref(&topic)).await;
        assert_eq!(hub.total_online(), 0);
        assert_eq!(hub.topic_online(&topic).await, 0);
    }
}
