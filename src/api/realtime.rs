use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};
use tokio::time::Instant;

use crate::model::enums::SekaiServerRegion;

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
    min_push_interval: Duration,
    push_throttle: StdMutex<HashMap<RealtimeTopic, TopicThrottle>>,
}

/// Per-topic `updated` push throttle state. Suppressed updates coalesce
/// into `pending` (latest timestamp wins) and one trailing task delivers
/// it when the window closes, so subscribers never miss the last change
/// of a burst — they just see it at most once per interval.
struct TopicThrottle {
    last_sent_at: Instant,
    pending: Option<i64>,
    trailing_scheduled: bool,
}

impl Default for RealtimeHub {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeHub {
    pub fn new() -> Self {
        Self::with_min_push_interval(Duration::ZERO)
    }

    /// `min_push_interval` caps how often an `updated` event is pushed per
    /// topic; `Duration::ZERO` pushes every update (the default).
    pub fn with_min_push_interval(min_push_interval: Duration) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                tx,
                online_total: AtomicUsize::new(0),
                online_by_topic: Mutex::new(HashMap::new()),
                min_push_interval,
                push_throttle: StdMutex::new(HashMap::new()),
            }),
        }
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

    pub fn notify_update(&self, topic: RealtimeTopic, timestamp: i64) {
        let interval = self.inner.min_push_interval;
        if interval.is_zero() {
            let _ = self
                .inner
                .tx
                .send(RealtimeMessage::Updated { topic, timestamp });
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
                    state.pending = Some(timestamp);
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
                let _ = self
                    .inner
                    .tx
                    .send(RealtimeMessage::Updated { topic, timestamp });
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
                    if let Some(timestamp) = pending {
                        let _ = inner.tx.send(RealtimeMessage::Updated { topic, timestamp });
                    }
                });
            }
        }
    }

    fn broadcast_online(&self, topic: RealtimeTopic, topic_online: usize) {
        let _ = self.inner.tx.send(RealtimeMessage::Online {
            topic,
            total: self.total_online(),
            topic_online,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_updated(msg: RealtimeMessage) -> (RealtimeTopic, i64) {
        match msg {
            RealtimeMessage::Updated { topic, timestamp } => (topic, timestamp),
            RealtimeMessage::Online { .. } => panic!("expected update message"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn throttles_update_pushes_with_trailing_coalescing() {
        let hub = RealtimeHub::with_min_push_interval(Duration::from_secs(5));
        let topic = RealtimeTopic::new(SekaiServerRegion::Jp, 7);
        let mut receiver = hub.subscribe();

        // First push of a window goes out immediately.
        hub.notify_update(topic.clone(), 1);
        assert_eq!(expect_updated(receiver.recv().await.unwrap()).1, 1);

        // Updates inside the window coalesce; the latest timestamp wins.
        hub.notify_update(topic.clone(), 2);
        hub.notify_update(topic.clone(), 3);
        assert!(receiver.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(expect_updated(receiver.recv().await.unwrap()).1, 3);

        // A quiet window resets to immediate delivery.
        tokio::time::advance(Duration::from_secs(6)).await;
        hub.notify_update(topic.clone(), 4);
        assert_eq!(expect_updated(receiver.recv().await.unwrap()).1, 4);
        // No stray trailing push follows.
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(receiver.try_recv().is_err());
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

        hub.notify_update(topic.clone(), 1234);
        match receiver.recv().await.unwrap() {
            RealtimeMessage::Updated {
                topic: received,
                timestamp,
            } => {
                assert_eq!(received, topic);
                assert_eq!(timestamp, 1234);
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
