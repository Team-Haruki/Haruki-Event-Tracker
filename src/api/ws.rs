use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sonic_rs::JsonValueTrait;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tower::ServiceExt;

use crate::api::access_log::ProxyTrust;
use crate::api::handler::private::PrivateSubject;
use crate::api::realtime::{RealtimeMessage, RealtimeTopic};
use crate::api::router::web_v2_routes;
use crate::api::state::AppState;
use crate::api::ws_ticket::{peer_from_connect_info, resolve_trusted_subject, unauthorized};
use crate::model::enums::SekaiServerRegion;

/// Client frames are small JSON commands; anything larger is a mistake or
/// abuse. tungstenite's default 128 KiB read buffer was zeroed per socket
/// and its 64 MiB message cap let any signed-in client pin that much.
const READ_BUFFER_BYTES: usize = 4096;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Proxied requests one socket runs at once, and how many more may wait
/// for a slot before the socket answers 429.
const MAX_INFLIGHT_PROXY: usize = 4;
const MAX_QUEUED_PROXY: usize = 64;

const OATHKEEPER_SUBJECT_HEADERS: &[&str] = &[
    "x-user-id",
    "x-authenticated-userid",
    "x-authenticated-user-id",
    "x-oathkeeper-subject",
    "x-ory-subject",
    "x-kratos-identity-id",
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsQuery {
    #[serde(default)]
    ticket: String,
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
    /// `updated` only: the event's API-cache epoch after this update. A
    /// client passes it as `v=<version>` to fetch the matching data over
    /// plain HTTP (cacheable, see `api::http_cache`).
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    online: Option<OnlinePayload>,
}

/// What goes back for one client frame: a structured response, or a
/// proxied success already rendered to its final wire text.
#[derive(Debug)]
enum WsReply {
    Response(WsResponse),
    Raw(String),
}

impl From<WsResponse> for WsReply {
    fn from(response: WsResponse) -> Self {
        Self::Response(response)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OnlinePayload {
    total: usize,
    topic: usize,
}

pub async fn connect(
    State((state, trust)): State<(AppState, Arc<ProxyTrust>)>,
    connect_info: ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let subject = if query.ticket.trim().is_empty() {
        if cfg!(debug_assertions) {
            resolve_trusted_subject(&headers, &trust, peer_from_connect_info(connect_info))
        } else {
            None
        }
    } else {
        state
            .ws_tickets()
            .consume(&query.ticket)
            .await
            .map(|ticket| ticket.subject)
    };
    let Some(subject) = subject else {
        return unauthorized().into_response();
    };

    ws.read_buffer_size(READ_BUFFER_BYTES)
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, trust, subject))
        .into_response()
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    trust: Arc<ProxyTrust>,
    subject: String,
) {
    // The routing table and middleware stack are identical for every
    // connection; build them once and hand out cheap clones.
    static WS_ROUTER: std::sync::OnceLock<Router> = std::sync::OnceLock::new();
    let router = WS_ROUTER
        .get_or_init(|| {
            Router::new()
                .merge(web_v2_routes(trust.clone()))
                .with_state(state.clone())
                .layer(axum::middleware::from_fn_with_state(
                    trust.clone(),
                    crate::api::access_log::log,
                ))
        })
        .clone();
    let hub = state.realtime().clone();
    let settings = *hub.settings();
    let mut rx = hub.subscribe();
    let mut topics: HashSet<RealtimeTopic> = HashSet::new();
    let total_online = hub.connection_opened();

    if send_event(
        &mut socket,
        &WsEvent {
            kind: "ready",
            subject: Some(subject.as_str()),
            server: None,
            event_id: None,
            timestamp: None,
            version: None,
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

    let mut proxy = ProxyPool::new(router, subject);
    let mut keepalive = Keepalive::new(settings.ws_ping_interval, settings.ws_idle_timeout);

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
                keepalive.saw_frame();
                let reply = match handle_client_message(&hub, &mut topics, message).await {
                    ClientAction::Reply(reply) => Some(reply),
                    ClientAction::Proxy(request) => proxy.submit(request),
                    ClientAction::Pong(payload) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                        None
                    }
                    ClientAction::Close => break,
                    ClientAction::Ignore => None,
                };
                if let Some(reply) = reply
                    && send_reply(&mut socket, reply).await.is_err()
                {
                    break;
                }
            }
            reply = proxy.next_reply(), if proxy.has_inflight() => {
                if let Some(text) = reply
                    && socket.send(Message::Text(text.into())).await.is_err()
                {
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
                        tracing::warn!(skipped, topics = topics.len(), "websocket realtime receiver lagged; resyncing subscribed topics");
                        if send_lagged_resync(&mut socket, &topics).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = keepalive.tick() => {
                if keepalive.idle() {
                    tracing::debug!("websocket idle; closing");
                    break;
                }
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
        }
    }

    let topics: Vec<RealtimeTopic> = topics.into_iter().collect();
    hub.connection_closed(&topics).await;
}

/// Server-side ping cadence plus the silence after which a socket is
/// dropped. Neither side used to send keepalives, so a half-open socket
/// stayed counted as online until its TCP state timed out.
struct Keepalive {
    ping: Option<Interval>,
    idle_timeout: Duration,
    last_seen: Instant,
}

impl Keepalive {
    fn new(ping_interval: Duration, idle_timeout: Duration) -> Self {
        let ping = (!ping_interval.is_zero()).then(|| {
            let mut interval =
                tokio::time::interval_at(Instant::now() + ping_interval, ping_interval);
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            interval
        });
        Self {
            ping,
            idle_timeout,
            last_seen: Instant::now(),
        }
    }

    fn saw_frame(&mut self) {
        self.last_seen = Instant::now();
    }

    fn idle(&self) -> bool {
        !self.idle_timeout.is_zero() && self.last_seen.elapsed() >= self.idle_timeout
    }

    async fn tick(&mut self) {
        match self.ping.as_mut() {
            Some(interval) => {
                interval.tick().await;
            }
            None => std::future::pending().await,
        }
    }
}

/// Proxied requests of one socket, run off the socket task so a slow
/// private detail no longer stalls the realtime feed (and the client's
/// other requests) behind it. Clients match replies by `id`, so replies
/// go back in completion order.
struct ProxyPool {
    router: Router,
    subject: Arc<str>,
    inflight: JoinSet<String>,
    queued: VecDeque<WsRequest>,
}

impl ProxyPool {
    fn new(router: Router, subject: String) -> Self {
        Self {
            router,
            subject: subject.into(),
            inflight: JoinSet::new(),
            queued: VecDeque::new(),
        }
    }

    fn has_inflight(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// Starts the request, or parks it until a slot frees up. Returns a
    /// reply to send right away only when the queue is full.
    fn submit(&mut self, request: WsRequest) -> Option<WsReply> {
        if self.inflight.len() < MAX_INFLIGHT_PROXY {
            self.spawn(request);
            return None;
        }
        if self.queued.len() >= MAX_QUEUED_PROXY {
            return Some(
                WsResponse::error(
                    &request.id,
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many pending requests",
                )
                .into(),
            );
        }
        self.queued.push_back(request);
        None
    }

    fn spawn(&mut self, request: WsRequest) {
        let router = self.router.clone();
        let subject = self.subject.clone();
        self.inflight.spawn(async move {
            handle_proxy_request(&router, request, &subject)
                .await
                .into_text()
        });
    }

    /// The next finished reply text. Pends while nothing is in flight;
    /// `None` when a task failed (logged; the client's own timeout covers
    /// that id).
    async fn next_reply(&mut self) -> Option<String> {
        let result = match self.inflight.join_next().await {
            Some(result) => result,
            None => std::future::pending().await,
        };
        if let Some(request) = self.queued.pop_front() {
            self.spawn(request);
        }
        match result {
            Ok(text) => Some(text),
            Err(err) => {
                tracing::error!(%err, "websocket proxy task failed");
                None
            }
        }
    }
}

/// What one client frame asks the socket task to do.
enum ClientAction {
    Reply(WsReply),
    Proxy(WsRequest),
    Pong(Bytes),
    Close,
    Ignore,
}

async fn handle_client_message(
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    message: Message,
) -> ClientAction {
    match message {
        Message::Text(text) => classify_text_request(hub, topics, text.as_str()).await,
        Message::Binary(bytes) => match std::str::from_utf8(&bytes) {
            Ok(text) => classify_text_request(hub, topics, text).await,
            Err(_) => ClientAction::Reply(
                WsResponse::error("", StatusCode::BAD_REQUEST, "invalid utf-8").into(),
            ),
        },
        Message::Ping(payload) => ClientAction::Pong(payload),
        Message::Pong(_) => ClientAction::Ignore,
        Message::Close(_) => ClientAction::Close,
    }
}

/// After the broadcast channel overran this socket, any subscribed topic
/// may have missed an `updated`. A version-less `updated` per topic makes
/// the client refetch over its uncached path; nothing else recovers a
/// missed push.
async fn send_lagged_resync(
    socket: &mut WebSocket,
    topics: &HashSet<RealtimeTopic>,
) -> Result<(), axum::Error> {
    let timestamp = chrono::Utc::now().timestamp();
    for topic in topics {
        send_event(
            socket,
            &WsEvent {
                kind: "updated",
                subject: None,
                server: Some(topic.server),
                event_id: Some(topic.event_id),
                timestamp: Some(timestamp),
                version: None,
                online: None,
            },
        )
        .await?;
    }
    Ok(())
}

async fn handle_realtime_message(
    socket: &mut WebSocket,
    topics: &HashSet<RealtimeTopic>,
    message: RealtimeMessage,
) -> Result<(), ()> {
    match message {
        RealtimeMessage::Updated {
            topic,
            timestamp,
            version,
        } => {
            if topics.contains(&topic) {
                send_event(
                    socket,
                    &WsEvent {
                        kind: "updated",
                        subject: None,
                        server: Some(topic.server),
                        event_id: Some(topic.event_id),
                        timestamp: Some(timestamp),
                        version,
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
                        version: None,
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

async fn classify_text_request(
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    text: &str,
) -> ClientAction {
    let request = match sonic_rs::from_str::<WsRequest>(text) {
        Ok(request) => request,
        Err(_) => {
            return ClientAction::Reply(
                WsResponse::error("", StatusCode::BAD_REQUEST, "invalid request").into(),
            );
        }
    };

    match request.kind.as_str() {
        "subscribe" => ClientAction::Reply(subscribe_topic(hub, topics, request).await.into()),
        "unsubscribe" => ClientAction::Reply(unsubscribe_topic(hub, topics, request).await.into()),
        "ping" => ClientAction::Reply(
            WsResponse {
                id: request.id,
                ok: true,
                data: sonic_rs::from_str(r#"{"type":"pong"}"#).ok(),
                error: None,
                status: StatusCode::OK.as_u16(),
            }
            .into(),
        ),
        _ => ClientAction::Proxy(request),
    }
}

/// The inline form of one frame's handling (proxied requests awaited in
/// place), for tests.
#[cfg(test)]
async fn handle_text_request(
    router: &Router,
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    subject: &str,
    text: &str,
) -> WsReply {
    match classify_text_request(hub, topics, text).await {
        ClientAction::Reply(reply) => reply,
        ClientAction::Proxy(request) => handle_proxy_request(router, request, subject).await,
        ClientAction::Pong(_) | ClientAction::Close | ClientAction::Ignore => {
            unreachable!("text frames never map to control actions")
        }
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

async fn handle_proxy_request(router: &Router, request: WsRequest, subject: &str) -> WsReply {
    if !is_allowed_event_path(&request.path) {
        return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "invalid path").into();
    }

    let uri: Uri = match request.path.parse() {
        Ok(uri) => uri,
        Err(_) => {
            return WsResponse::error(&request.id, StatusCode::BAD_REQUEST, "invalid path").into();
        }
    };
    let mut http_request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .expect("valid websocket proxy request");
    http_request
        .extensions_mut()
        .insert(PrivateSubject(subject.to_owned()));

    let response = match router.clone().oneshot(http_request).await {
        Ok(response) => response,
        Err(err) => {
            tracing::error!(%err, path = %request.path, "websocket proxy request failed");
            return WsResponse::error(
                &request.id,
                StatusCode::INTERNAL_SERVER_ERROR,
                "request failed",
            )
            .into();
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
            )
            .into();
        }
    };

    if !status.is_success() {
        let message = extract_error_message(&body).unwrap_or_else(|| status.to_string());
        return WsResponse::error(&request.id, status, message).into();
    }

    match splice_success(&request.id, status, &body) {
        Some(text) => WsReply::Raw(text),
        None => WsResponse::error(
            &request.id,
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid json response",
        )
        .into(),
    }
}

/// Renders `{"id":…,"ok":true,"data":<body>,"status":…}` around the
/// handler's JSON body as-is — the same text `WsResponse` serialization
/// produced from a parsed copy, without building and re-encoding a tree
/// for every (often ~128 KB) overview. The body is only validated.
fn splice_success(id: &str, status: StatusCode, body: &[u8]) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    sonic_rs::from_str::<sonic_rs::LazyValue<'_>>(body).ok()?;
    let id = sonic_rs::to_string(id).ok()?;
    let mut text = String::with_capacity(body.len() + id.len() + 40);
    text.push_str(r#"{"id":"#);
    text.push_str(&id);
    text.push_str(r#","ok":true,"data":"#);
    text.push_str(body);
    text.push_str(r#","status":"#);
    text.push_str(&status.as_u16().to_string());
    text.push('}');
    Some(text)
}

pub fn resolve_oathkeeper_subject(headers: &HeaderMap) -> Option<String> {
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

async fn send_reply(socket: &mut WebSocket, reply: WsReply) -> Result<(), axum::Error> {
    socket.send(Message::Text(reply.into_text().into())).await
}

impl WsReply {
    fn into_text(self) -> String {
        match self {
            Self::Raw(text) => text,
            Self::Response(response) => match sonic_rs::to_string(&response) {
                Ok(text) => text,
                Err(err) => {
                    tracing::error!(%err, "websocket response encode failed");
                    r#"{"id":"","ok":false,"error":"json encode error","status":500}"#.to_owned()
                }
            },
        }
    }
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
    if !path.starts_with("/api/v2/web/") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::realtime::RealtimeHub;
    use crate::api::state::AppState;
    use crate::api::ws_ticket::WsTicketStore;
    use crate::config::ApiQueryConfig;
    use crate::privacy::UidAnonymizer;
    use axum::Json;
    use axum::routing::get;
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use std::collections::HashMap;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::Message as ClientMessage;

    #[derive(Debug, Deserialize)]
    struct ParsedReply {
        ok: bool,
        status: u16,
        #[serde(default)]
        data: Option<sonic_rs::Value>,
        #[serde(default)]
        error: Option<String>,
    }

    impl WsReply {
        fn parsed(self) -> ParsedReply {
            sonic_rs::from_str(&self.into_text()).unwrap()
        }
    }

    fn router() -> Router {
        Router::new()
            .route(
                "/api/v2/web/ok",
                get(|| async { Json(json!({"value": 42})) }),
            )
            .route(
                "/api/v2/web/error",
                get(|| async {
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"error": "bad query"})),
                    )
                }),
            )
            .route(
                "/api/v2/web/text",
                get(|| async { (StatusCode::OK, "not json") }),
            )
    }

    fn state() -> AppState {
        state_with(RealtimeHub::new())
    }

    fn state_with(hub: RealtimeHub) -> AppState {
        AppState::new(
            HashMap::new(),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), []),
            UidAnonymizer::disabled(),
            None,
            hub,
            WsTicketStore::default(),
        )
    }

    /// Serves `/ws` for `state` on a loopback port; returns the address.
    async fn serve(state: AppState) -> std::net::SocketAddr {
        let (trust, invalid) = ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
        assert!(invalid.is_empty());
        let app = Router::new().route("/ws", get(connect).with_state((state, Arc::new(trust))));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        address
    }

    async fn connect_as(
        address: std::net::SocketAddr,
        subject: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
        request
            .headers_mut()
            .insert("x-oathkeeper-subject", subject.parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
    }

    /// A router whose `/api/v2/web/slow` route sleeps `delay` and records
    /// the peak number of concurrently running calls.
    fn slow_router(delay: Duration, peak: Arc<std::sync::atomic::AtomicUsize>) -> Router {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let running = Arc::new(AtomicUsize::new(0));
        router().route(
            "/api/v2/web/slow",
            get(move || {
                let running = running.clone();
                let peak = peak.clone();
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    Json(json!({"slow": true}))
                }
            }),
        )
    }

    fn proxy_request(id: &str, path: &str) -> WsRequest {
        WsRequest {
            id: id.into(),
            path: path.into(),
            kind: String::new(),
            server: None,
            event_id: None,
        }
    }

    #[tokio::test]
    async fn proxy_pool_caps_concurrency_and_replies_in_completion_order() {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut pool = ProxyPool::new(
            slow_router(Duration::from_millis(60), peak.clone()),
            "owner".into(),
        );
        assert!(!pool.has_inflight());
        assert!(
            pool.submit(proxy_request("slow-0", "/api/v2/web/slow"))
                .is_none()
        );
        assert!(
            pool.submit(proxy_request("fast", "/api/v2/web/ok"))
                .is_none()
        );
        for i in 1..6 {
            assert!(
                pool.submit(proxy_request(&format!("slow-{i}"), "/api/v2/web/slow"))
                    .is_none()
            );
        }
        assert!(pool.has_inflight());

        // The fast request is not stuck behind the slow one sent first.
        let first: ParsedReply = sonic_rs::from_str(&pool.next_reply().await.unwrap()).unwrap();
        assert!(first.ok);
        assert_eq!(first.data.unwrap()["value"].as_i64(), Some(42));
        let mut slow_replies = 0;
        while pool.has_inflight() {
            if let Some(text) = pool.next_reply().await {
                let reply: ParsedReply = sonic_rs::from_str(&text).unwrap();
                assert!(reply.ok);
                slow_replies += 1;
            }
        }
        assert_eq!(slow_replies, 6);
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            MAX_INFLIGHT_PROXY
        );
    }

    #[tokio::test]
    async fn proxy_pool_rejects_requests_beyond_the_queue() {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut pool = ProxyPool::new(slow_router(Duration::from_secs(30), peak), "owner".into());
        for i in 0..(MAX_INFLIGHT_PROXY + MAX_QUEUED_PROXY) {
            assert!(
                pool.submit(proxy_request(&format!("r{i}"), "/api/v2/web/slow"))
                    .is_none()
            );
        }
        let rejected = pool
            .submit(proxy_request("overflow", "/api/v2/web/slow"))
            .expect("the queue is full")
            .parsed();
        assert!(!rejected.ok);
        assert_eq!(rejected.status, StatusCode::TOO_MANY_REQUESTS.as_u16());
        assert_eq!(pool.queued.len(), MAX_QUEUED_PROXY);
        // Dropping the pool aborts what is in flight.
    }

    #[tokio::test]
    async fn keepalive_pings_responsive_sockets_and_closes_silent_ones() {
        use crate::api::realtime::RealtimeSettings;
        let state = state_with(RealtimeHub::with_settings(RealtimeSettings {
            ws_ping_interval: Duration::from_millis(40),
            ws_idle_timeout: Duration::from_millis(100),
            ..RealtimeSettings::immediate()
        }));
        let hub = state.realtime().clone();
        let address = serve(state).await;

        // A client that keeps reading answers pings (tokio-tungstenite
        // does so on read) and stays connected well past the idle timeout.
        let mut alive = connect_as(address, "alive").await;
        assert_eq!(next_json(&mut alive).await["type"].as_str(), Some("ready"));
        let mut pings = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, alive.next()).await {
                Ok(Some(Ok(ClientMessage::Ping(_)))) => pings += 1,
                Ok(Some(Ok(ClientMessage::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                    panic!("responsive socket was closed")
                }
                Ok(Some(Ok(_))) => {}
                Err(_) => break,
            }
        }
        assert!(pings >= 3, "{pings}");
        assert_eq!(hub.total_online(), 1);
        // Keep answering pings in the background while the silent socket
        // is exercised; a client that stops reading is silent too.
        let alive_closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let alive_task = tokio::spawn({
            let alive_closed = alive_closed.clone();
            async move {
                while let Some(Ok(message)) = alive.next().await {
                    if matches!(message, ClientMessage::Close(_)) {
                        break;
                    }
                }
                alive_closed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        // A client that never reads never pongs; the server drops it.
        let mut silent = connect_as(address, "silent").await;
        assert_eq!(hub.total_online(), 2);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut closed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match tokio::time::timeout_at(deadline, silent.next()).await {
                Ok(Some(Ok(ClientMessage::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                    closed = true;
                    break;
                }
                Ok(Some(Ok(_))) => {}
                Err(_) => break,
            }
        }
        assert!(closed, "silent socket was not closed");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hub.total_online(), 1);
        assert!(!alive_closed.load(std::sync::atomic::Ordering::SeqCst));
        alive_task.abort();
    }

    #[tokio::test]
    async fn lagged_receivers_resync_their_subscribed_topics() {
        let state = state();
        let hub = state.realtime().clone();
        let address = serve(state).await;
        let mut socket = connect_as(address, "viewer").await;
        assert_eq!(next_json(&mut socket).await["type"].as_str(), Some("ready"));
        socket
            .send(ClientMessage::Text(
                r#"{"id":"sub","type":"subscribe","server":"jp","eventId":99}"#.into(),
            ))
            .await
            .unwrap();
        for _ in 0..2 {
            next_json(&mut socket).await;
        }

        // Flood another topic without yielding: the socket task's receiver
        // overruns the 1024-message channel before it can drain it.
        let other = RealtimeTopic::new(SekaiServerRegion::En, 1);
        for i in 0..2000 {
            hub.notify_update(other.clone(), i, Some(i));
        }
        let resync = next_json(&mut socket).await;
        assert_eq!(resync["type"].as_str(), Some("updated"));
        assert_eq!(resync["server"].as_str(), Some("jp"));
        assert_eq!(resync["eventId"].as_i64(), Some(99));
        assert!(resync["timestamp"].as_i64().is_some());
        assert!(resync.get("version").is_none());

        // The socket is still serving: nothing from the flooded topic
        // leaks through and the next frame is the ping reply.
        socket
            .send(ClientMessage::Text(r#"{"id":"p","type":"ping"}"#.into()))
            .await
            .unwrap();
        assert_eq!(next_json(&mut socket).await["id"].as_str(), Some("p"));
        socket.close(None).await.unwrap();
    }

    async fn next_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> sonic_rs::Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            if let ClientMessage::Text(text) = message {
                return sonic_rs::from_str(&text).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn text_requests_manage_topics_and_ping() {
        let router = router();
        let hub = crate::api::realtime::RealtimeHub::new();
        let mut topics = HashSet::new();
        hub.connection_opened();

        let invalid = handle_text_request(&router, &hub, &mut topics, "owner", "{")
            .await
            .parsed();
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST.as_u16());

        let missing = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"1","type":"subscribe"}"#,
        )
        .await
        .parsed();
        assert!(!missing.ok);

        let missing_event = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"2","type":"subscribe","server":"jp"}"#,
        )
        .await
        .parsed();
        assert!(!missing_event.ok);

        let subscribed = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"3","type":"subscribe","server":"jp","eventId":10}"#,
        )
        .await
        .parsed();
        assert!(subscribed.ok);
        assert_eq!(topics.len(), 1);

        let duplicate = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"4","type":"subscribe","server":"jp","eventId":10}"#,
        )
        .await
        .parsed();
        assert!(duplicate.ok);
        assert_eq!(
            hub.topic_online(&RealtimeTopic::new(SekaiServerRegion::Jp, 10))
                .await,
            1
        );

        let ping = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"5","type":"ping"}"#,
        )
        .await
        .parsed();
        assert!(ping.ok);
        assert_eq!(ping.data.unwrap()["type"].as_str(), Some("pong"));

        let missing_unsubscribe = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"6","type":"unsubscribe"}"#,
        )
        .await
        .parsed();
        assert!(!missing_unsubscribe.ok);

        let invalid_unsubscribe = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"7","type":"unsubscribe","server":"jp","eventId":0}"#,
        )
        .await
        .parsed();
        assert!(!invalid_unsubscribe.ok);

        let unsubscribed = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner",
            r#"{"id":"8","type":"unsubscribe","server":"jp","eventId":10}"#,
        )
        .await
        .parsed();
        assert!(unsubscribed.ok);
        assert!(topics.is_empty());
        hub.connection_closed(&[]).await;
    }

    #[tokio::test]
    async fn proxy_requests_validate_paths_status_and_json() {
        let router = router();
        let hub = crate::api::realtime::RealtimeHub::new();
        let mut topics = HashSet::new();

        let ok = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner-1",
            r#"{"id":"1","path":"/api/v2/web/ok"}"#,
        )
        .await
        .parsed();
        assert!(ok.ok);
        assert_eq!(ok.data.unwrap()["value"].as_i64(), Some(42));

        let error = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner-1",
            r#"{"id":"2","path":"/api/v2/web/error"}"#,
        )
        .await
        .parsed();
        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY.as_u16());
        assert_eq!(error.error.as_deref(), Some("bad query"));

        let invalid_json = handle_text_request(
            &router,
            &hub,
            &mut topics,
            "owner-1",
            r#"{"id":"3","path":"/api/v2/web/text"}"#,
        )
        .await
        .parsed();
        assert_eq!(
            invalid_json.status,
            StatusCode::INTERNAL_SERVER_ERROR.as_u16()
        );

        for path in [
            "/other/path",
            "/api/v2/web/http://evil.test",
            "/api/v2/web/bad\\path",
            "/api/v2/web/bad\npath",
        ] {
            let request = WsRequest {
                id: "bad".into(),
                path: path.into(),
                kind: String::new(),
                server: None,
                event_id: None,
            };
            let response = handle_proxy_request(&router, request, "owner-1")
                .await
                .parsed();
            assert_eq!(response.status, StatusCode::BAD_REQUEST.as_u16());
        }
    }

    /// The pre-splice encoding: parse the body, re-serialize it inside
    /// `WsResponse`.
    fn parsed_success_text(id: &str, status: StatusCode, body: &[u8]) -> String {
        sonic_rs::to_string(&WsResponse {
            id: id.to_owned(),
            ok: true,
            data: Some(sonic_rs::from_slice(body).unwrap()),
            error: None,
            status: status.as_u16(),
        })
        .unwrap()
    }

    #[test]
    fn spliced_replies_match_the_parsed_encoding_byte_for_byte() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        use crate::api::handler::web::tests::{
                            NORMAL_EVENT, WORLD_BLOOM_EVENT, test_state,
                        };
                        let state = test_state(true).await;
                        let user = state.anonymizer().public_user_id(
                            SekaiServerRegion::Jp,
                            NORMAL_EVENT,
                            "100",
                        );
                        let (trust, _) =
                            ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
                        let router = web_v2_routes(Arc::new(trust)).with_state(state);
                        let base = format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards");
                        let wb = format!(
                            "/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17"
                        );
                        let paths = [
                            format!("{base}/total/overview?at=1710000060&interval=60"),
                            format!("{base}/total/replay/overview?at=1710000060&interval=60"),
                            format!("{base}/total/details/rank/1?at=1710000060&includeTrace=true"),
                            format!("{base}/total/details/user/{user}?at=1710000060"),
                            format!("{base}/total/users/search?name=Alpha"),
                            format!("{base}/total/check-room?userId=100"),
                            format!("{wb}/overview?at=1710000060&interval=60"),
                            format!("{wb}/users/search?name=Alpha"),
                        ];
                        for path in paths {
                            for id in ["1", "quo\"te\\back/slash", "\u{1}ユニ"] {
                                let body = axum::body::to_bytes(
                                    router
                                        .clone()
                                        .oneshot(
                                            Request::builder()
                                                .uri(&path)
                                                .body(Body::empty())
                                                .unwrap(),
                                        )
                                        .await
                                        .unwrap()
                                        .into_body(),
                                    usize::MAX,
                                )
                                .await
                                .unwrap();
                                let request = WsRequest {
                                    id: id.into(),
                                    path: path.clone(),
                                    kind: String::new(),
                                    server: None,
                                    event_id: None,
                                };
                                let WsReply::Raw(text) =
                                    handle_proxy_request(&router, request, "owner").await
                                else {
                                    panic!("{path} was not spliced");
                                };
                                assert_eq!(
                                    text,
                                    parsed_success_text(id, StatusCode::OK, &body),
                                    "{path}"
                                );
                            }
                        }
                    });
            })
            .unwrap()
            .join()
            .unwrap();

        // Whatever sonic-rs emits — escapes, floats, unicode, nesting —
        // splices to the same text a parse + re-encode produces.
        let body = sonic_rs::to_vec(&json!({
            "s": "\"q\" \\ / \n \t \u{7f} \u{2028} é ユニ 🎉",
            "f": [0.1, 1.5e300, 123456789.125, f64::MIN_POSITIVE],
            "i": [i64::MIN, u64::MAX],
            "n": null,
            "b": [true, false],
            "e": {"a": [], "o": {}},
        }))
        .unwrap();
        assert_eq!(
            splice_success("x", StatusCode::OK, &body).unwrap(),
            parsed_success_text("x", StatusCode::OK, &body)
        );
        // The one divergence: the old parse dropped the sign of `-0.0`;
        // the splice keeps the handler's bytes, as the HTTP body does.
        assert_eq!(
            splice_success("x", StatusCode::OK, b"[-0.0]").unwrap(),
            r#"{"id":"x","ok":true,"data":[-0.0],"status":200}"#
        );
        for invalid in [
            &b"not json"[..],
            b"{\"a\":tru}",
            b"{\"a\":1",
            b"[1,]",
            b"{} {}",
            b"\xff",
        ] {
            assert!(
                splice_success("x", StatusCode::OK, invalid).is_none(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn subject_and_error_helpers_cover_header_aliases() {
        let mut headers = HeaderMap::new();
        headers.insert("x-user-id", "  user-1  ".parse().unwrap());
        assert_eq!(
            resolve_oathkeeper_subject(&headers).as_deref(),
            Some("user-1")
        );
        headers.clear();
        headers.insert("x-user", "fallback".parse().unwrap());
        assert_eq!(
            resolve_oathkeeper_subject(&headers).as_deref(),
            Some("fallback")
        );
        headers.clear();
        assert!(resolve_oathkeeper_subject(&headers).is_none());

        assert!(is_allowed_event_path("/api/v2/web/events/jp/1"));
        assert!(!is_allowed_event_path("/api/v2/cloud/events/jp/1"));
        assert_eq!(
            extract_error_message(br#"{"error":"boom"}"#).as_deref(),
            Some("boom")
        );
        assert!(extract_error_message(b"not-json").is_none());
    }

    #[tokio::test]
    async fn websocket_session_handles_frames_and_realtime_events() {
        let state = state();
        let hub = state.realtime().clone();
        let (trust, invalid) = ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
        assert!(invalid.is_empty());
        let app = Router::new().route(
            "/ws",
            get(connect).with_state((state.clone(), Arc::new(trust))),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
        request
            .headers_mut()
            .insert("x-oathkeeper-subject", "identity-1".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let ready = next_json(&mut socket).await;
        assert_eq!(ready["type"].as_str(), Some("ready"));

        socket
            .send(ClientMessage::Ping(vec![1, 2].into()))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Pong(_)
        ));

        socket
            .send(ClientMessage::Text(
                r#"{"id":"sub","type":"subscribe","server":"jp","eventId":99}"#.into(),
            ))
            .await
            .unwrap();
        let mut saw_subscribed = false;
        let mut saw_online = false;
        for _ in 0..2 {
            let message = next_json(&mut socket).await;
            saw_subscribed |=
                message["id"].as_str() == Some("sub") && message["ok"].as_bool() == Some(true);
            saw_online |= message["type"].as_str() == Some("online");
        }
        assert!(saw_subscribed && saw_online);

        hub.notify_update(
            RealtimeTopic::new(SekaiServerRegion::Jp, 99),
            1234,
            Some(42),
        );
        let updated = next_json(&mut socket).await;
        assert_eq!(updated["type"].as_str(), Some("updated"));
        assert_eq!(updated["timestamp"].as_i64(), Some(1234));
        assert_eq!(updated["version"].as_i64(), Some(42));
        // Without an API cache there is no version to announce.
        hub.notify_update(RealtimeTopic::new(SekaiServerRegion::Jp, 99), 1235, None);
        let unversioned = next_json(&mut socket).await;
        assert_eq!(unversioned["timestamp"].as_i64(), Some(1235));
        assert!(unversioned.get("version").is_none());

        socket
            .send(ClientMessage::Binary(vec![0xff].into()))
            .await
            .unwrap();
        assert_eq!(next_json(&mut socket).await["status"].as_i64(), Some(400));
        socket
            .send(ClientMessage::Binary(
                br#"{"id":"binary","type":"ping"}"#.to_vec().into(),
            ))
            .await
            .unwrap();
        assert_eq!(next_json(&mut socket).await["id"].as_str(), Some("binary"));
        socket
            .send(ClientMessage::Pong(Vec::new().into()))
            .await
            .unwrap();

        socket
            .send(ClientMessage::Text(
                r#"{"id":"unsub","type":"unsubscribe","server":"jp","eventId":99}"#.into(),
            ))
            .await
            .unwrap();
        let unsubscribed = next_json(&mut socket).await;
        assert_eq!(unsubscribed["id"].as_str(), Some("unsub"));
        socket.close(None).await.unwrap();

        let ticket = state.ws_tickets().issue("ticket-owner".into()).await;
        let url = format!("ws://{address}/ws?ticket={}", ticket.ticket);
        let (mut ticket_socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        assert_eq!(
            next_json(&mut ticket_socket).await["subject"].as_str(),
            Some("ticket-owner")
        );
        ticket_socket.close(None).await.unwrap();

        assert_eq!(
            hub.topic_online(&RealtimeTopic::new(SekaiServerRegion::Jp, 99))
                .await,
            0
        );
        server.abort();
    }
}
