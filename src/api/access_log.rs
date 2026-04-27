//! Access-log middleware. Format mirrors fiber's
//! `${time} | ${status} | ${latency} | ${ip} | ${method} ${path}`
//! so existing log-shipping pipelines keep working unchanged.
//!
//! IP resolution: prefers `X-Forwarded-For` (first hop) when present,
//! otherwise falls back to the connection peer address that
//! `into_make_service_with_connect_info::<SocketAddr>()` injects via
//! request extensions. Falls back to `-` when neither is available.

use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::ConnectInfo;
use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use chrono::Local;

pub async fn log(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());

    let ip = client_ip(&req);

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

fn client_ip(req: &Request) -> String {
    if let Some(xff) = req.headers().get(header::FORWARDED).or_else(|| {
        req.headers()
            .get("x-forwarded-for")
            .or_else(|| req.headers().get("x-real-ip"))
    }) {
        if let Ok(s) = xff.to_str() {
            if let Some(first) = s.split(',').next() {
                let trimmed = first.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_owned();
                }
            }
        }
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "-".to_owned())
}
