use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sonic_rs::JsonValueTrait;
use tokio::sync::broadcast;
use tower::ServiceExt;

use crate::api::access_log::ProxyTrust;
use crate::api::realtime::{RealtimeMessage, RealtimeTopic};
use crate::api::router::event_routes;
use crate::api::state::AppState;
use crate::model::enums::SekaiServerRegion;

const OATHKEEPER_SUBJECT_HEADERS: &[&str] = &[
    "x-user-id",
    "x-authenticated-userid",
    "x-authenticated-user-id",
    "x-oathkeeper-subject",
    "x-ory-subject",
    "x-user",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsRequest {
    id: String,
    #[serde(default)]
    path: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    server: Option<SekaiServerRegion>,
    #[serde(default)]
    event_id: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WsResponse {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<sonic_rs::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    status: u16,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WsEvent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<SekaiServerRegion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    online: Option<OnlinePayload>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OnlinePayload {
    total: usize,
    topic: usize,
}

pub async fn connect(
    State((state, trust)): State<(AppState, Arc<ProxyTrust>)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let subject = resolve_oathkeeper_subject(&headers);
    ws.on_upgrade(move |socket| handle_socket(socket, state, trust, subject))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    trust: Arc<ProxyTrust>,
    subject: Option<String>,
) {
    let router = Router::new()
        .nest("/event/{server}/{event_id}", event_routes())
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            trust,
            crate::api::access_log::log,
        ));
    let hub = state.realtime().clone();
    let mut rx = hub.subscribe();
    let mut topics: HashSet<RealtimeTopic> = HashSet::new();
    let total_online = hub.connection_opened();

    if send_event(
        &mut socket,
        &WsEvent {
            kind: "ready",
            subject: subject.as_deref(),
            server: None,
            event_id: None,
            timestamp: None,
            online: Some(OnlinePayload {
                total: total_online,
                topic: 0,
            }),
        },
    )
    .await
    .is_err()
    {
        hub.connection_closed(&[]).await;
        return;
    }

    loop {
        tokio::select! {
            message = socket.recv() => {
                let Some(message) = message else {
                    break;
                };
                let message = match message {
                    Ok(message) => message,
                    Err(err) => {
                        tracing::debug!(%err, "websocket receive failed");
                        break;
                    }
                };
                if handle_client_message(&mut socket, &router, &hub, &mut topics, message).await.is_err() {
                    break;
                }
            }
            message = rx.recv() => {
                match message {
                    Ok(message) => {
                        if handle_realtime_message(&mut socket, &topics, message).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "websocket realtime receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    let topics: Vec<RealtimeTopic> = topics.into_iter().collect();
    hub.connection_closed(&topics).await;
}

async fn handle_client_message(
    socket: &mut WebSocket,
    router: &Router,
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    message: Message,
) -> Result<(), ()> {
    let response = match message {
        Message::Text(text) => handle_text_request(router, hub, topics, text.as_str()).await,
        Message::Binary(bytes) => match std::str::from_utf8(&bytes) {
            Ok(text) => handle_text_request(router, hub, topics, text).await,
            Err(_) => WsResponse::error("", StatusCode::BAD_REQUEST, "invalid utf-8"),
        },
        Message::Ping(payload) => return socket.send(Message::Pong(payload)).await.map_err(|_| ()),
        Message::Pong(_) => return Ok(()),
        Message::Close(_) => return Err(()),
    };

    send_response(socket, &response).await.map_err(|_| ())
}

async fn handle_realtime_message(
    socket: &mut WebSocket,
    topics: &HashSet<RealtimeTopic>,
    message: RealtimeMessage,
) -> Result<(), ()> {
    match message {
        RealtimeMessage::Updated { topic, timestamp } => {
            if topics.contains(&topic) {
                send_event(
                    socket,
                    &WsEvent {
                        kind: "updated",
                        subject: None,
                        server: Some(topic.server),
                        event_id: Some(topic.event_id),
                        timestamp: Some(timestamp),
                        online: None,
                    },
                )
                .await
                .map_err(|_| ())?;
            }
        }
        RealtimeMessage::Online {
            topic,
            total,
            topic_online,
        } => {
            if topics.contains(&topic) {
                send_event(
                    socket,
                    &WsEvent {
                        kind: "online",
                        subject: None,
                        server: Some(topic.server),
                        event_id: Some(topic.event_id),
                        timestamp: None,
                        online: Some(OnlinePayload {
                            total,
                            topic: topic_online,
                        }),
                    },
                )
                .await
                .map_err(|_| ())?;
            }
        }
    }

    Ok(())
}

async fn handle_text_request(
    router: &Router,
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    text: &str,
) -> WsResponse {
    let request = match sonic_rs::from_str::<WsRequest>(text) {
        Ok(request) => request,
        Err(_) => return WsResponse::error("", StatusCode::BAD_REQUEST, "invalid request"),
    };

    match request.kind.as_str() {
        "subscribe" => subscribe_topic(hub, topics, request).await,
        "unsubscribe" => unsubscribe_topic(hub, topics, request).await,
        "ping" => WsResponse {
            id: request.id,
            ok: true,
            data: sonic_rs::from_str(r#"{"type":"pong"}"#).ok(),
            error: None,
            status: StatusCode::OK.as_u16(),
        },
        _ => handle_proxy_request(router, request).await,
    }
}

async fn unsubscribe_topic(
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    request: WsRequest,
) -> WsResponse {
    let Some(server) = request.server else {
        return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "server is required");
    };
    let Some(event_id) = request.event_id.filter(|event_id| *event_id > 0) else {
        return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "eventId is required");
    };
    let topic = RealtimeTopic::new(server, event_id);
    if topics.remove(&topic) {
        hub.remove_topic_subscription(&topic).await;
    }
    let online = hub.topic_online(&topic).await;
    let data = sonic_rs::to_value(&OnlinePayload {
        total: hub.total_online(),
        topic: online,
    })
    .ok();
    WsResponse {
        id: request.id,
        ok: true,
        data,
        error: None,
        status: StatusCode::OK.as_u16(),
    }
}

async fn subscribe_topic(
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    request: WsRequest,
) -> WsResponse {
    let Some(server) = request.server else {
        return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "server is required");
    };
    let Some(event_id) = request.event_id.filter(|event_id| *event_id > 0) else {
        return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "eventId is required");
    };
    let topic = RealtimeTopic::new(server, event_id);
    if topics.insert(topic.clone()) {
        hub.add_topic_subscription(topic.clone()).await;
    }
    let online = hub.topic_online(&topic).await;
    let data = sonic_rs::to_value(&OnlinePayload {
        total: hub.total_online(),
        topic: online,
    })
    .ok();
    WsResponse {
        id: request.id,
        ok: true,
        data,
        error: None,
        status: StatusCode::OK.as_u16(),
    }
}

async fn handle_proxy_request(router: &Router, request: WsRequest) -> WsResponse {
    if !is_allowed_event_path(&request.path) {
        return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "invalid path");
    }

    let uri: Uri = match request.path.parse() {
        Ok(uri) => uri,
        Err(_) => return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "invalid path"),
    };
    let http_request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .expect("valid websocket proxy request");

    let response = match router.clone().oneshot(http_request).await {
        Ok(response) => response,
        Err(err) => {
            tracing::error!(%err, path = %request.path, "websocket proxy request failed");
            return WsResponse::error(
                &request.id,
                StatusCode::INTERNAL_SERVER_ERROR,
                "request failed",
            );
        }
    };
    let status = response.status();
    let body = match axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024).await {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(%err, path = %request.path, "websocket proxy body read failed");
            return WsResponse::error(
                &request.id,
                StatusCode::INTERNAL_SERVER_ERROR,
                "request failed",
            );
        }
    };

    if !status.is_success() {
        let message = extract_error_message(&body).unwrap_or_else(|| status.to_string());
        return WsResponse::error(&request.id, status, message);
    }

    match sonic_rs::from_slice::<sonic_rs::Value>(&body) {
        Ok(data) => WsResponse {
            id: request.id,
            ok: true,
            data: Some(data),
            error: None,
            status: status.as_u16(),
        },
        Err(_) => WsResponse::error(
            &request.id,
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid json response",
        ),
    }
}

fn resolve_oathkeeper_subject(headers: &HeaderMap) -> Option<String> {
    for name in OATHKEEPER_SUBJECT_HEADERS {
        if let Some(value) = headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_owned());
        }
    }

    None
}

async fn send_response(socket: &mut WebSocket, response: &WsResponse) -> Result<(), axum::Error> {
    let text = match sonic_rs::to_string(response) {
        Ok(text) => text,
        Err(err) => {
            tracing::error!(%err, "websocket response encode failed");
            r#"{"id":"","ok":false,"error":"json encode error","status":500}"#.to_owned()
        }
    };
    socket.send(Message::Text(text.into())).await
}

async fn send_event(socket: &mut WebSocket, event: &WsEvent<'_>) -> Result<(), axum::Error> {
    let text = match sonic_rs::to_string(event) {
        Ok(text) => text,
        Err(err) => {
            tracing::error!(%err, "websocket event encode failed");
            return Ok(());
        }
    };
    socket.send(Message::Text(text.into())).await
}

fn is_allowed_event_path(path: &str) -> bool {
    if !path.starts_with("/event/") {
        return false;
    }
    !path.contains("://") && !path.contains('\\') && !path.contains('\n') && !path.contains('\r')
}

fn extract_error_message(body: &[u8]) -> Option<String> {
    sonic_rs::from_slice::<sonic_rs::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_owned)
        })
}

impl WsResponse {
    fn error(id: &str, status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            ok: false,
            data: None,
            error: Some(message.into()),
            status: status.as_u16(),
        }
    }
}
