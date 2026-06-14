use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

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
}

impl Default for RealtimeHub {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                tx,
                online_total: AtomicUsize::new(0),
                online_by_topic: Mutex::new(HashMap::new()),
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
        let _ = self
            .inner
            .tx
            .send(RealtimeMessage::Updated { topic, timestamp });
    }

    fn broadcast_online(&self, topic: RealtimeTopic, topic_online: usize) {
        let _ = self.inner.tx.send(RealtimeMessage::Online {
            topic,
            total: self.total_online(),
            topic_online,
        });
    }
}
