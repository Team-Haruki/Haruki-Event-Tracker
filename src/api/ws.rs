use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sonic_rs::JsonValueTrait;
use tokio::sync::broadcast;
use tower::ServiceExt;

use crate::api::access_log::ProxyTrust;
use crate::api::handler::private::PrivateSubject;
use crate::api::realtime::{RealtimeMessage, RealtimeTopic};
use crate::api::router::web_v2_routes;
use crate::api::state::AppState;
use crate::api::ws_ticket::{peer_from_connect_info, resolve_trusted_subject, unauthorized};
use crate::model::enums::SekaiServerRegion;

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

    ws.on_upgrade(move |socket| handle_socket(socket, state, trust, subject))
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
                if handle_client_message(&mut socket, &router, &hub, &mut topics, &subject, message).await.is_err() {
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
    subject: &str,
    message: Message,
) -> Result<(), ()> {
    let response = match message {
        Message::Text(text) => {
            handle_text_request(router, hub, topics, subject, text.as_str()).await
        }
        Message::Binary(bytes) => match std::str::from_utf8(&bytes) {
            Ok(text) => handle_text_request(router, hub, topics, subject, text).await,
            Err(_) => WsResponse::error("", StatusCode::BAD_REQUEST, "invalid utf-8").into(),
        },
        Message::Ping(payload) => return socket.send(Message::Pong(payload)).await.map_err(|_| ()),
        Message::Pong(_) => return Ok(()),
        Message::Close(_) => return Err(()),
    };

    send_reply(socket, response).await.map_err(|_| ())
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

async fn handle_text_request(
    router: &Router,
    hub: &crate::api::realtime::RealtimeHub,
    topics: &mut HashSet<RealtimeTopic>,
    subject: &str,
    text: &str,
) -> WsReply {
    let request = match sonic_rs::from_str::<WsRequest>(text) {
        Ok(request) => request,
        Err(_) => return WsResponse::error("", StatusCode::BAD_REQUEST, "invalid request").into(),
    };

    match request.kind.as_str() {
        "subscribe" => subscribe_topic(hub, topics, request).await.into(),
        "unsubscribe" => unsubscribe_topic(hub, topics, request).await.into(),
        "ping" => WsResponse {
            id: request.id,
            ok: true,
            data: sonic_rs::from_str(r#"{"type":"pong"}"#).ok(),
            error: None,
            status: StatusCode::OK.as_u16(),
        }
        .into(),
        _ => handle_proxy_request(router, request, subject).await,
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
        AppState::new(
            HashMap::new(),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), []),
            UidAnonymizer::disabled(),
            None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
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
