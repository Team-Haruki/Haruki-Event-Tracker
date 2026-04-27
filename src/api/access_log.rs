//! Access-log middleware. Format mirrors fiber's
//! `${time} | ${status} | ${latency} | ${ip} | ${method} ${path}`
//! so existing log-shipping pipelines keep working unchanged.
//!
//! IP resolution honours `enable_trust_proxy` / `trusted_proxies` /
//! `proxy_header` from `BackendConfig`. When trust-proxy is on and the
//! TCP peer is inside one of the configured CIDRs, the configured
//! `proxy_header` (default `X-Forwarded-For`) is consulted first, then
//! `X-Real-IP`, then `Forwarded`; otherwise the peer address is used
//! verbatim. Header values are taken as the **leftmost** comma-separated
//! token, matching fiber's behaviour.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ConnectInfo;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use chrono::Local;
use ipnet::IpNet;

#[derive(Debug, Clone)]
pub struct ProxyTrust {
    pub enabled: bool,
    pub trusted: Vec<IpNet>,
    /// Lower-cased header name to honour first when the peer is trusted.
    pub primary_header: String,
}

impl ProxyTrust {
    pub fn from_config(
        enabled: bool,
        trusted_cidrs: &[String],
        proxy_header: &str,
    ) -> (Self, Vec<String>) {
        let mut parsed = Vec::with_capacity(trusted_cidrs.len());
        let mut bad = Vec::new();
        for raw in trusted_cidrs {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            match raw.parse::<IpNet>() {
                Ok(net) => parsed.push(net),
                Err(_) => match raw.parse::<IpAddr>() {
                    Ok(ip) => parsed.push(IpNet::from(ip)),
                    Err(_) => bad.push(raw.to_owned()),
                },
            }
        }
        let header = if proxy_header.trim().is_empty() {
            "x-forwarded-for".to_owned()
        } else {
            proxy_header.trim().to_ascii_lowercase()
        };
        (
            ProxyTrust {
                enabled,
                trusted: parsed,
                primary_header: header,
            },
            bad,
        )
    }

    fn peer_is_trusted(&self, peer: &IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(peer))
    }
}

pub async fn log(
    State(trust): State<Arc<ProxyTrust>>,
    req: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());

    let ip = client_ip(&req, &trust);

    let response = next.run(req).await;

    let status = response.status().as_u16();
    let latency = started.elapsed();
    let now = Local::now().format("%Y/%m/%d %H:%M:%S");
    tracing::info!(
        target: "access",
        "{} | {:>3} | {:>10?} | {} | {} {}",
        now, status, latency, ip, method, path
    );
    response
}

fn first_token(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).and_then(|s| {
        s.split(',')
            .next()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_owned())
    })
}

fn client_ip(req: &Request, trust: &ProxyTrust) -> String {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());

    if trust.enabled && peer.map(|p| trust.peer_is_trusted(&p)).unwrap_or(false) {
        let headers = req.headers();
        if let Some(ip) = first_token(headers, &trust.primary_header) {
            return ip;
        }
        // Common fallbacks the Go version also accepts.
        for h in ["x-real-ip", "forwarded"] {
            if let Some(ip) = first_token(headers, h) {
                return ip;
            }
        }
    }

    peer.map(|p| p.to_string()).unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderName, HeaderValue, Request as HttpRequest};

    fn make_req(peer: &str, headers: &[(&str, &str)]) -> Request {
        let mut req = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes()).unwrap();
            req.headers_mut().insert(name, HeaderValue::from_str(v).unwrap());
        }
        let ip: IpAddr = peer.parse().unwrap();
        let addr = SocketAddr::new(ip, 1234);
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    fn trust(enabled: bool, cidrs: &[&str], header: &str) -> ProxyTrust {
        let owned: Vec<String> = cidrs.iter().map(|s| (*s).to_owned()).collect();
        let (t, bad) = ProxyTrust::from_config(enabled, &owned, header);
        assert!(bad.is_empty(), "unparseable cidrs in test: {bad:?}");
        t
    }

    #[test]
    fn untrusted_peer_ignores_forwarded_header() {
        let t = trust(true, &["10.0.0.0/8"], "X-Forwarded-For");
        let req = make_req("8.8.8.8", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_ip(&req, &t), "8.8.8.8");
    }

    #[test]
    fn trusted_peer_uses_forwarded_first_token() {
        let t = trust(true, &["127.0.0.0/8"], "X-Forwarded-For");
        let req = make_req("127.0.0.1", &[("x-forwarded-for", "1.2.3.4, 5.6.7.8")]);
        assert_eq!(client_ip(&req, &t), "1.2.3.4");
    }

    #[test]
    fn trusted_peer_falls_back_to_x_real_ip() {
        let t = trust(true, &["127.0.0.0/8"], "X-Forwarded-For");
        let req = make_req("127.0.0.1", &[("x-real-ip", "9.9.9.9")]);
        assert_eq!(client_ip(&req, &t), "9.9.9.9");
    }

    #[test]
    fn disabled_trust_always_uses_peer() {
        let t = trust(false, &["127.0.0.0/8"], "X-Forwarded-For");
        let req = make_req("127.0.0.1", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_ip(&req, &t), "127.0.0.1");
    }

    #[test]
    fn unparseable_cidrs_are_collected_not_panic() {
        let cidrs = vec!["10.0.0.0/8".to_owned(), "not-a-cidr".to_owned(), "::1".to_owned()];
        let (t, bad) = ProxyTrust::from_config(true, &cidrs, "");
        assert_eq!(t.trusted.len(), 2);
        assert_eq!(bad, vec!["not-a-cidr"]);
        assert_eq!(t.primary_header, "x-forwarded-for"); // empty -> default
    }

    #[test]
    fn ipv6_loopback_in_cidr_works() {
        let t = trust(true, &["::1/128"], "X-Forwarded-For");
        let req = make_req("::1", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_ip(&req, &t), "1.2.3.4");
    }
}

