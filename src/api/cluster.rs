//! `/internal/*` — the writer's update stream (see `crate::cluster`) and the
//! master-registry webhook.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::api::json::Json;
use crate::api::state::AppState;
use crate::cluster::{StreamMessage, bearer_token, token_matches};
use crate::model::enums::SekaiServerRegion;

/// Body of the master registry's `POST <subscriber>/internal/master-updated`
/// (`registry.subscribers` in Haruki-Sekai-API). Same shape a SekaiAPI peer
/// accepts, so the registry needs no tracker-specific case.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterUpdatedNotice {
    pub server: String,
    #[serde(default)]
    pub data_version: String,
}

#[derive(serde::Serialize)]
struct MasterUpdatedResponse {
    server: SekaiServerRegion,
    invalidated: bool,
}

/// Drop the tracker's cached `events.json` / `worldBlooms.json` for the
/// region so the next tick re-reads them from the registry instead of
/// waiting out the stat throttle. Regions without a daemon here answer 200
/// with `invalidated: false` — the registry fans out to every subscriber
/// regardless of which regions it tracks.
pub async fn master_updated(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(notice): axum::Json<MasterUpdatedNotice>,
) -> Response {
    if !bearer_token(&headers).is_some_and(|token| token_matches(token, state.cluster_token())) {
        return (StatusCode::UNAUTHORIZED, "cluster token required").into_response();
    }
    let Some(server) = SekaiServerRegion::parse(&notice.server) else {
        return (StatusCode::BAD_REQUEST, "unknown server").into_response();
    };
    let invalidated = match state.master_parser(server) {
        Some(parser) => {
            parser.invalidate();
            tracing::info!(%server, data_version = %notice.data_version, "master data cache invalidated by registry");
            true
        }
        None => false,
    };
    Json(MasterUpdatedResponse {
        server,
        invalidated,
    })
    .into_response()
}

pub async fn updates(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(bus) = state.update_bus().cloned() else {
        return (StatusCode::NOT_FOUND, "no update stream on this node").into_response();
    };
    if !bearer_token(&headers).is_some_and(|token| token_matches(token, state.cluster_token())) {
        return (StatusCode::UNAUTHORIZED, "cluster token required").into_response();
    }
    let ping_interval = state.cluster_ping_interval();
    ws.on_upgrade(move |socket| stream(socket, bus, ping_interval))
        .into_response()
}

async fn stream(
    mut socket: WebSocket,
    bus: crate::cluster::UpdateBus,
    ping_interval: std::time::Duration,
) {
    let mut rx = bus.subscribe();
    if send(
        &mut socket,
        &StreamMessage::Hello {
            seq: bus.current_seq(),
        },
    )
    .await
    .is_err()
    {
        return;
    }
    let mut ping = tokio::time::interval(ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    loop {
        tokio::select! {
            event = rx.recv() => {
                let message = match event {
                    Ok(event) => event.into_message(),
                    Err(RecvError::Lagged(_)) => {
                        // The reader resyncs on a mid-stream hello.
                        StreamMessage::Hello { seq: bus.current_seq() }
                    }
                    Err(RecvError::Closed) => return,
                };
                if send(&mut socket, &message).await.is_err() {
                    return;
                }
            }
            _ = ping.tick() => {
                if send(&mut socket, &StreamMessage::Ping).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    // Readers only ever answer pings; anything else is ignored.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

async fn send(socket: &mut WebSocket, message: &StreamMessage) -> Result<(), axum::Error> {
    let text = sonic_rs::to_string(message).unwrap_or_default();
    socket.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::Message;

    use super::*;
    use crate::api::access_log::ProxyTrust;
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::realtime::{RealtimeHub, RealtimeMessage};
    use crate::api::router::build_router;
    use crate::api::state::ClusterState;
    use crate::api::ws_ticket::WsTicketStore;
    use crate::cluster::subscriber::{SubscriberConfig, SubscriberDeps, run};
    use crate::cluster::{ClusterLink, UpdateBus};
    use crate::config::{ApiQueryConfig, ClusterRole};
    use crate::model::enums::SekaiServerRegion;
    use crate::privacy::UidAnonymizer;

    async fn writer_server(bus: UpdateBus) -> String {
        let state = AppState::new(
            HashMap::new(),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), []),
            UidAnonymizer::enabled("salt"),
            None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
        .with_cluster(ClusterState {
            role: ClusterRole::Writer,
            cluster_token: "secret".into(),
            ping_interval: Some(Duration::from_millis(50)),
            update_bus: Some(bus),
            ..ClusterState::default()
        });
        let (trust, _) = ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
        let router = build_router(state, Arc::new(trust));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        format!("http://{addr}")
    }

    async fn next_message<S>(socket: &mut S) -> StreamMessage
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("frame before timeout")
                .expect("stream open")
                .expect("frame ok");
            if let Message::Text(text) = frame {
                return sonic_rs::from_str(&text).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn stream_authenticates_and_replays_bus_events() {
        let bus = UpdateBus::new();
        let base = writer_server(bus.clone()).await;
        let url = format!(
            "ws://{}/internal/updates",
            base.trim_start_matches("http://")
        );

        // No token → the handshake is rejected.
        let anon =
            tokio_tungstenite::connect_async(url.as_str().into_client_request().unwrap()).await;
        assert!(anon.is_err());

        let mut request = url.as_str().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer secret".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(
            next_message(&mut socket).await,
            StreamMessage::Hello { seq: 0 }
        );

        bus.publish(SekaiServerRegion::Cn, 179, 42, Some("0/1".into()));
        assert_eq!(
            next_message(&mut socket).await,
            StreamMessage::Updated {
                seq: 1,
                server: SekaiServerRegion::Cn,
                event_id: 179,
                timestamp: 42,
                lsn: Some("0/1".into()),
            }
        );
        // Keepalive pings arrive on the configured interval.
        assert_eq!(next_message(&mut socket).await, StreamMessage::Ping);
        socket.send(Message::Close(None)).await.unwrap();
    }

    #[tokio::test]
    async fn master_updated_webhook_requires_the_token_and_invalidates_parsers() {
        use crate::tracker::parser::EventDataParser;
        let dir = std::env::temp_dir().join(format!("tracker-webhook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.json"), "[]").unwrap();
        std::fs::write(dir.join("worldBlooms.json"), "[]").unwrap();
        let parser = EventDataParser::new(SekaiServerRegion::Cn, dir.to_str().unwrap()).unwrap();
        parser.load_event_data().await.unwrap();

        let state = AppState::new(
            HashMap::new(),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), []),
            UidAnonymizer::enabled("salt"),
            None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
        .with_cluster(ClusterState {
            role: ClusterRole::Writer,
            cluster_token: "secret".into(),
            update_bus: Some(UpdateBus::new()),
            master_parsers: HashMap::from([(SekaiServerRegion::Cn, parser.clone())]),
            ..ClusterState::default()
        });
        let (trust, _) = ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
        let router = build_router(state, Arc::new(trust));
        let post = |token: Option<&str>, body: &str| {
            let mut req = axum::http::Request::builder()
                .method("POST")
                .uri("/internal/master-updated")
                .header("content-type", "application/json");
            if let Some(token) = token {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            req.body(axum::body::Body::from(body.to_owned())).unwrap()
        };
        use tower::ServiceExt;
        let body = r#"{"server":"cn","dataVersion":"6.0.0.1"}"#;
        assert_eq!(
            router
                .clone()
                .oneshot(post(None, body))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let ok = router
            .clone()
            .oneshot(post(Some("secret"), body))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(ok.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], br#"{"server":"cn","invalidated":true}"#);
        // The cache was dropped: a re-read hits the file again (bypassing
        // the 5 s stat throttle) and still parses.
        std::fs::write(dir.join("events.json"), "[]").unwrap();
        parser.load_event_data().await.unwrap();

        let other = router
            .clone()
            .oneshot(post(Some("secret"), r#"{"server":"jp"}"#))
            .await
            .unwrap();
        assert_eq!(other.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(other.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], br#"{"server":"jp","invalidated":false}"#);
        assert_eq!(
            router
                .oneshot(post(Some("secret"), r#"{"server":"xx"}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn reader_subscriber_applies_updates_to_the_local_realtime_hub() {
        let bus = UpdateBus::new();
        let base = writer_server(bus.clone()).await;
        let realtime = RealtimeHub::new();
        let mut rx = realtime.subscribe();
        let link = Arc::new(ClusterLink::default());
        tokio::spawn(run(
            SubscriberConfig {
                writer_url: base,
                token: "secret".into(),
                replica_wait: Duration::ZERO,
                reconnect_min: Duration::from_millis(50),
                reconnect_max: Duration::from_millis(100),
            },
            SubscriberDeps {
                dbs: HashMap::new(),
                api_cache_redis: None,
                realtime: realtime.clone(),
                link: link.clone(),
            },
        ));
        // Wait for the hello before publishing so the event is not missed.
        for _ in 0..100 {
            if link.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(link.is_connected());
        bus.publish(SekaiServerRegion::Jp, 200, 7, None);
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            message,
            RealtimeMessage::Updated { ref topic, timestamp: 7, version: None }
                if topic.server == SekaiServerRegion::Jp && topic.event_id == 200
        ));
        assert_eq!(link.last_seq(), 1);
    }
}
