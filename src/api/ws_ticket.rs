use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use rand::Rng;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::api::access_log::ProxyTrust;
use crate::api::json::Json;

const TICKET_BYTES: usize = 32;
const TICKET_TTL: Duration = Duration::from_secs(45);

#[derive(Clone, Default)]
pub struct WsTicketStore {
    inner: Arc<Mutex<HashMap<String, WsTicket>>>,
}

#[derive(Clone)]
pub struct WsTicket {
    pub subject: String,
    expires_at: Instant,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WsTicketResponse {
    pub ticket: String,
    pub expires_in: u64,
}

impl WsTicketStore {
    pub async fn issue(&self, subject: String) -> WsTicketResponse {
        let ticket = generate_ticket();
        let expires_at = Instant::now() + TICKET_TTL;
        let mut tickets = self.inner.lock().await;
        prune_expired(&mut tickets, Instant::now());
        tickets.insert(
            ticket.clone(),
            WsTicket {
                subject,
                expires_at,
            },
        );
        WsTicketResponse {
            ticket,
            expires_in: TICKET_TTL.as_secs(),
        }
    }

    pub async fn consume(&self, ticket: &str) -> Option<WsTicket> {
        let ticket = ticket.trim();
        if ticket.is_empty() {
            return None;
        }

        let mut tickets = self.inner.lock().await;
        let now = Instant::now();
        prune_expired(&mut tickets, now);
        let ticket = tickets.remove(ticket)?;
        (ticket.expires_at > now).then_some(ticket)
    }
}

pub fn resolve_trusted_subject(
    headers: &HeaderMap,
    trust: &ProxyTrust,
    peer: Option<std::net::SocketAddr>,
) -> Option<String> {
    if !trust.enabled && !cfg!(debug_assertions) {
        return None;
    }

    if trust.enabled {
        let peer_ip = peer.map(|addr| addr.ip())?;
        if !trust.peer_is_trusted(&peer_ip) {
            return None;
        }
    }

    super::ws::resolve_oathkeeper_subject(headers)
}

pub fn unauthorized() -> (StatusCode, &'static str) {
    (StatusCode::UNAUTHORIZED, "login required")
}

pub async fn issue_ticket(
    axum::extract::State((state, trust)): axum::extract::State<(
        crate::api::state::AppState,
        Arc<ProxyTrust>,
    )>,
    connect_info: ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let subject = resolve_trusted_subject(&headers, &trust, peer_from_connect_info(connect_info));
    let Some(subject) = subject else {
        return unauthorized().into_response();
    };

    Json(state.ws_tickets().issue(subject).await).into_response()
}

fn generate_ticket() -> String {
    let mut bytes = [0_u8; TICKET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn prune_expired(tickets: &mut HashMap<String, WsTicket>, now: Instant) {
    tickets.retain(|_, ticket| ticket.expires_at > now);
}

pub fn peer_from_connect_info(
    connect_info: ConnectInfo<std::net::SocketAddr>,
) -> Option<std::net::SocketAddr> {
    Some(connect_info.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::realtime::RealtimeHub;
    use crate::api::state::AppState;
    use crate::config::ApiQueryConfig;
    use crate::privacy::UidAnonymizer;
    use axum::http::{HeaderMap, HeaderValue};
    use axum::response::IntoResponse;
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr};

    fn trust(enabled: bool, cidrs: &[&str]) -> ProxyTrust {
        let owned: Vec<String> = cidrs.iter().map(|value| (*value).to_owned()).collect();
        let (trust, bad) = ProxyTrust::from_config(enabled, &owned, "X-Forwarded-For", 1.0, 1000);
        assert!(bad.is_empty(), "unparseable cidrs in test: {bad:?}");
        trust
    }

    #[tokio::test]
    async fn ticket_can_only_be_consumed_once() {
        let store = WsTicketStore::default();
        let issued = store.issue("subject-1".to_owned()).await;

        let first = store.consume(&issued.ticket).await;
        assert_eq!(
            first.as_ref().map(|ticket| ticket.subject.as_str()),
            Some("subject-1")
        );
        assert!(store.consume(&issued.ticket).await.is_none());
    }

    #[test]
    fn trusted_subject_requires_trusted_peer_when_proxy_trust_is_enabled() {
        let trust = trust(true, &["127.0.0.0/8"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-oathkeeper-subject", HeaderValue::from_static("user-1"));

        assert_eq!(
            resolve_trusted_subject(
                &headers,
                &trust,
                Some(SocketAddr::new(IpAddr::from([127, 0, 0, 1]), 3456)),
            ),
            Some("user-1".to_owned()),
        );
        assert_eq!(
            resolve_trusted_subject(
                &headers,
                &trust,
                Some(SocketAddr::new(IpAddr::from([8, 8, 8, 8]), 3456)),
            ),
            None,
        );
    }

    #[tokio::test]
    async fn ticket_helpers_cover_empty_expired_and_handler_paths() {
        let store = WsTicketStore::default();
        assert!(store.consume("   ").await.is_none());
        store.inner.lock().await.insert(
            "expired".into(),
            WsTicket {
                subject: "old".into(),
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        );
        assert!(store.consume("expired").await.is_none());
        let generated = generate_ticket();
        assert_eq!(generated.len(), TICKET_BYTES * 2);
        assert!(generated.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(unauthorized().0, StatusCode::UNAUTHORIZED);

        let state = AppState::new(
            HashMap::new(),
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), []),
            UidAnonymizer::disabled(),
            None,
            RealtimeHub::new(),
            store,
        );
        let trust = Arc::new(trust(false, &[]));
        let mut headers = HeaderMap::new();
        headers.insert("x-oathkeeper-subject", HeaderValue::from_static("user-2"));
        let peer = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), 1234);
        assert_eq!(peer_from_connect_info(ConnectInfo(peer)), Some(peer));
        let response = issue_ticket(
            axum::extract::State((state, trust)),
            ConnectInfo(peer),
            headers,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
