//! HTTP caching headers for the public web group (`/api/v2/web/...`).
//!
//! Every `200` gets a strong `ETag` (a digest of the exact bytes on the
//! wire, so identity / gzip / br representations never share a tag) and
//! `If-None-Match` answers `304`. `Cache-Control` depends on what the
//! response is:
//!
//! - `private, no-store` for anything tied to a person: the `/private/`
//!   routes and raw-UID lookups (handlers mark those with [`private`]).
//! - `public, max-age=86400, immutable` when the request carries
//!   `v=<epoch>` and the bytes are the ones the API cache holds for exactly
//!   that epoch ([`ServedEpoch`]). A version's data never changes, so the
//!   URL can be cached anywhere.
//! - `public, max-age=1, stale-while-revalidate=5` for everything else,
//!   including a `v` that doesn't match what was served.
//!
//! `v` is never part of the server-side cache key: handlers ignore it and
//! the key already contains the epoch.
//!
//! The layer sits outside `CompressionLayer`, so it hashes (and counts
//! `Content-Length` of) what is actually sent. WebSocket request frames
//! run the web routes without this layer and are unaffected.

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, ETAG, IF_NONE_MATCH, VARY};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

pub const LIVE_CACHE_CONTROL: &str = "public, max-age=1, stale-while-revalidate=5";
pub const VERSIONED_CACHE_CONTROL: &str = "public, max-age=86400, immutable";
pub const PRIVATE_CACHE_CONTROL: &str = "private, no-store";
const UNCACHEABLE_CACHE_CONTROL: &str = "no-store";
const WEB_PREFIX: &str = "/api/v2/web/";
/// Web responses are at most a few MB; this only bounds a runaway body.
const MAX_BUFFERED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Response extension: the API-cache epoch the body's bytes belong to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServedEpoch(pub i64);

/// Response extension: never let a shared cache keep this response.
#[derive(Clone, Copy, Debug)]
pub struct PrivateResponse;

pub fn private(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response.extensions_mut().insert(PrivateResponse);
    response
}

pub async fn web_cache_headers(req: Request, next: Next) -> Response {
    if req.method() != Method::GET || !req.uri().path().starts_with(WEB_PREFIX) {
        return next.run(req).await;
    }
    let requested_version = requested_version(req.uri().query());
    let if_none_match = req.headers().get(IF_NONE_MATCH).cloned();
    let private_route = req.uri().path().contains("/private/");

    let response = next.run(req).await;
    if private_route || response.extensions().get::<PrivateResponse>().is_some() {
        return with_cache_control(response, PRIVATE_CACHE_CONTROL);
    }
    if response.status() != StatusCode::OK {
        return with_cache_control(response, UNCACHEABLE_CACHE_CONTROL);
    }

    let immutable = requested_version.is_some()
        && response.extensions().get::<ServedEpoch>().map(|e| e.0) == requested_version;
    let cache_control = if immutable {
        VERSIONED_CACHE_CONTROL
    } else {
        LIVE_CACHE_CONTROL
    };

    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_BUFFERED_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(%err, "web response body read failed");
            return with_cache_control(
                StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                UNCACHEABLE_CACHE_CONTROL,
            );
        }
    };
    let etag = strong_etag(&bytes);
    parts.headers.insert(ETAG, etag.clone());
    parts
        .headers
        .insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    ensure_vary_accept_encoding(&mut parts.headers);

    if if_none_match.is_some_and(|value| if_none_match_hits(&value, &etag)) {
        let mut not_modified = StatusCode::NOT_MODIFIED.into_response();
        let headers = not_modified.headers_mut();
        for name in [ETAG, CACHE_CONTROL, VARY] {
            for value in parts.headers.get_all(&name) {
                headers.append(name.clone(), value.clone());
            }
        }
        return not_modified;
    }

    parts
        .headers
        .insert(CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    Response::from_parts(parts, Body::from(bytes))
}

fn with_cache_control(mut response: Response, value: &'static str) -> Response {
    let headers = response.headers_mut();
    headers.remove(ETAG);
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

fn requested_version(query: Option<&str>) -> Option<i64> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "v")
        .and_then(|(_, value)| value.parse::<i64>().ok())
}

fn strong_etag(bytes: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(bytes);
    let mut tag = String::with_capacity(34);
    tag.push('"');
    for byte in &digest[..16] {
        tag.push_str(&format!("{byte:02x}"));
    }
    tag.push('"');
    HeaderValue::from_str(&tag).expect("hex etag is a valid header value")
}

/// `If-None-Match` uses weak comparison (RFC 9110 §13.1.2): a `W/` prefix
/// on the client's copy still matches.
fn if_none_match_hits(value: &HeaderValue, etag: &HeaderValue) -> bool {
    let (Ok(value), Ok(etag)) = (value.to_str(), etag.to_str()) else {
        return false;
    };
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

fn ensure_vary_accept_encoding(headers: &mut HeaderMap) {
    let present = headers.get_all(VARY).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|name| name.trim().eq_ignore_ascii_case("accept-encoding"))
        })
    });
    if !present {
        headers.append(VARY, HeaderValue::from_static("accept-encoding"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::access_log::ProxyTrust;
    use crate::api::cache::{ApiCache, finish_event_update};
    use crate::api::handler::web::tests::{
        NORMAL_EVENT, WORLD_BLOOM_EVENT, test_state, test_state_with_cache,
    };
    use crate::api::json::EncodedJson;
    use crate::api::router::build_router;
    use crate::config::ApiCacheConfig;
    use crate::model::enums::SekaiServerRegion;
    use axum::Router;
    use axum::routing::get;
    use bytes::Bytes;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn trust() -> Arc<ProxyTrust> {
        let (trust, invalid) = ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
        assert!(invalid.is_empty());
        Arc::new(trust)
    }

    async fn get_with(router: &Router, uri: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap())
    }

    async fn body(response: Response) -> Bytes {
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
    }

    /// Runs `f` on a current-thread runtime with a large stack, like the
    /// router tests: the full handler futures are deep in debug builds.
    fn run<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()>,
    {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(f())
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn parses_versions_and_matches_if_none_match() {
        assert_eq!(requested_version(None), None);
        assert_eq!(requested_version(Some("interval=60&v=42")), Some(42));
        assert_eq!(requested_version(Some("v=abc&interval=60")), None);
        assert_eq!(requested_version(Some("vv=1&_t=2")), None);

        let etag = strong_etag(b"payload");
        assert_eq!(etag, strong_etag(b"payload"));
        assert_ne!(etag, strong_etag(b"payload2"));
        let tag = etag.to_str().unwrap().to_owned();
        assert_eq!(tag.len(), 34);
        for value in [
            tag.clone(),
            format!("W/{tag}"),
            format!("\"other\", {tag}"),
            "*".to_owned(),
        ] {
            assert!(
                if_none_match_hits(&HeaderValue::from_str(&value).unwrap(), &etag),
                "{value}"
            );
        }
        assert!(!if_none_match_hits(
            &HeaderValue::from_static("\"other\""),
            &etag
        ));
    }

    fn epoch_router() -> Router {
        Router::new()
            .route(
                "/api/v2/web/versioned",
                get(|| async {
                    EncodedJson::identity(Bytes::from_static(br#"{"n":1}"#)).at_epoch(Some(7))
                }),
            )
            .route(
                "/api/v2/web/unversioned",
                get(|| async { EncodedJson::identity(Bytes::from_static(br#"{"n":2}"#)) }),
            )
            .route(
                "/api/v2/cloud/other",
                get(|| async { EncodedJson::identity(Bytes::from_static(br#"{"n":3}"#)) }),
            )
            .layer(axum::middleware::from_fn(web_cache_headers))
    }

    #[tokio::test]
    async fn immutable_only_when_v_matches_the_served_epoch() {
        let router = epoch_router();
        for (uri, expected) in [
            ("/api/v2/web/versioned?v=7", VERSIONED_CACHE_CONTROL),
            (
                "/api/v2/web/versioned?interval=60&v=7",
                VERSIONED_CACHE_CONTROL,
            ),
            // A stale (or future) v still gets the current bytes, but only
            // with the short lifetime.
            ("/api/v2/web/versioned?v=6", LIVE_CACHE_CONTROL),
            ("/api/v2/web/versioned?v=8", LIVE_CACHE_CONTROL),
            ("/api/v2/web/versioned", LIVE_CACHE_CONTROL),
            ("/api/v2/web/unversioned?v=7", LIVE_CACHE_CONTROL),
        ] {
            let response = get_with(&router, uri, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(header(&response, "cache-control"), Some(expected), "{uri}");
        }
        let versioned = get_with(&router, "/api/v2/web/versioned?v=7", &[]).await;
        let stale = get_with(&router, "/api/v2/web/versioned?v=6", &[]).await;
        assert_eq!(header(&versioned, "etag"), header(&stale, "etag"));
        assert_eq!(body(versioned).await, body(stale).await);

        // Non-web routes are left alone.
        let cloud = get_with(&router, "/api/v2/cloud/other", &[]).await;
        assert!(header(&cloud, "etag").is_none());
        assert!(header(&cloud, "cache-control").is_none());
    }

    #[test]
    fn web_routes_get_etags_and_answer_if_none_match_with_304() {
        run(|| async {
            let router = build_router(test_state(true).await, trust());
            // A pinned `at` keeps the (uncached) overview byte-stable.
            let uri = format!(
                "/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/overview?interval=60&at=1710000060"
            );
            let first = get_with(&router, &uri, &[]).await;
            assert_eq!(first.status(), StatusCode::OK);
            assert_eq!(header(&first, "cache-control"), Some(LIVE_CACHE_CONTROL));
            assert_eq!(header(&first, "vary"), Some("accept-encoding"));
            let etag = header(&first, "etag").unwrap().to_owned();
            assert!(etag.starts_with('"') && etag.ends_with('"'));
            let length: usize = header(&first, "content-length").unwrap().parse().unwrap();
            let first_body = body(first).await;
            assert_eq!(first_body.len(), length);

            // `v` never reaches the cache key or the payload.
            let versioned = get_with(&router, &format!("{uri}&v=12345"), &[]).await;
            assert_eq!(header(&versioned, "etag"), Some(etag.as_str()));
            assert_eq!(
                header(&versioned, "cache-control"),
                Some(LIVE_CACHE_CONTROL)
            );
            assert_eq!(body(versioned).await, first_body);

            let not_modified = get_with(&router, &uri, &[("if-none-match", &etag)]).await;
            assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(header(&not_modified, "etag"), Some(etag.as_str()));
            assert_eq!(
                header(&not_modified, "cache-control"),
                Some(LIVE_CACHE_CONTROL)
            );
            assert_eq!(header(&not_modified, "vary"), Some("accept-encoding"));
            assert!(body(not_modified).await.is_empty());

            let weak = format!("\"nope\", W/{etag}");
            let weak_hit = get_with(&router, &uri, &[("if-none-match", &weak)]).await;
            assert_eq!(weak_hit.status(), StatusCode::NOT_MODIFIED);
            let miss = get_with(&router, &uri, &[("if-none-match", "\"nope\"")]).await;
            assert_eq!(miss.status(), StatusCode::OK);

            // Every representation has its own tag: the compressed bytes
            // are what gets hashed.
            let search = format!(
                "/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/users/search?name=Alpha"
            );
            let plain = get_with(&router, &search, &[]).await;
            let gzip = get_with(&router, &search, &[("accept-encoding", "gzip")]).await;
            assert_eq!(plain.status(), StatusCode::OK);
            assert_eq!(gzip.status(), StatusCode::OK);
            if header(&gzip, "content-encoding").is_some() {
                assert_ne!(header(&plain, "etag"), header(&gzip, "etag"));
                let gzip_etag = header(&gzip, "etag").unwrap().to_owned();
                let revalidated = get_with(
                    &router,
                    &search,
                    &[("accept-encoding", "gzip"), ("if-none-match", &gzip_etag)],
                )
                .await;
                assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
            }
        });
    }

    #[test]
    fn private_and_error_responses_are_never_stored() {
        run(|| async {
            let state = test_state(true).await;
            let router = build_router(state, trust());
            let base = format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards");
            for (path, status) in [
                // Raw-UID lookups answer with the UID itself.
                (format!("{base}/total/details/user/100"), StatusCode::OK),
                (
                    format!("{base}/total/details/user/100?idType=uid"),
                    StatusCode::OK,
                ),
                (
                    format!("{base}/total/check-room?userId=100"),
                    StatusCode::OK,
                ),
                // The private group, even when it refuses.
                (
                    format!("{base}/total/private/details/user/100"),
                    StatusCode::UNAUTHORIZED,
                ),
            ] {
                let response = get_with(&router, &path, &[]).await;
                assert_eq!(response.status(), status, "{path}");
                assert_eq!(
                    header(&response, "cache-control"),
                    Some(PRIVATE_CACHE_CONTROL),
                    "{path}"
                );
                assert!(header(&response, "etag").is_none(), "{path}");
            }

            let missing = get_with(&router, &format!("{base}/total/details/rank/0"), &[]).await;
            assert!(missing.status().is_client_error());
            assert_eq!(
                header(&missing, "cache-control"),
                Some(UNCACHEABLE_CACHE_CONTROL)
            );
            assert!(header(&missing, "etag").is_none());
        });
    }

    #[test]
    fn versioned_parts_are_immutable_epoch_pure_and_consistent() {
        run(|| async {
            let Ok(url) = std::env::var("HARUKI_COVERAGE_REDIS_URL") else {
                return;
            };
            let client = redis::Client::open(url).unwrap();
            let mut conn = redis::aio::ConnectionManager::new(client).await.unwrap();
            let cache = ApiCache::new(
                vec![conn.clone()],
                ApiCacheConfig {
                    enabled: true,
                    precompress_min_bytes: 1,
                    // Short TTLs so a version is recomputed within the test.
                    latest_rank_ttl_secs: 1,
                    local_value_ttl_ms: 100,
                    ..ApiCacheConfig::default()
                },
            );
            let state = test_state_with_cache(true, Some(cache)).await;
            let router = build_router(state, trust());
            let epoch = finish_event_update(&mut conn, SekaiServerRegion::Jp, NORMAL_EVENT)
                .await
                .unwrap();
            let wb_epoch = finish_event_update(&mut conn, SekaiServerRegion::Jp, WORLD_BLOOM_EVENT)
                .await
                .unwrap();
            let base = format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total");
            let part = |name: &str, v: i64| format!("{base}/{name}?interval=60&v={v}");
            let gzip = [("accept-encoding", "gzip, br")];

            // Fresh fetch (accepted into the cache), then a cache hit.
            let mut first = Vec::new();
            for name in ["top100", "borders", "growth"] {
                for _ in 0..2 {
                    let response = get_with(&router, &part(name, epoch), &gzip).await;
                    assert_eq!(response.status(), StatusCode::OK, "{name}");
                    assert_eq!(header(&response, "content-encoding"), Some("gzip"));
                    assert_eq!(
                        header(&response, "cache-control"),
                        Some(VERSIONED_CACHE_CONTROL),
                        "{name}"
                    );
                }
                let plain = get_with(&router, &part(name, epoch), &[]).await;
                // Identity bodies are served too, but never immutable.
                assert_eq!(header(&plain, "cache-control"), Some(LIVE_CACHE_CONTROL));
                first.push((name, body(plain).await));
            }

            // All parts of one version share one as-of time: the newest
            // sample, not the wall clock.
            let metas: Vec<serde_json::Value> = first
                .iter()
                .map(|(_, bytes)| {
                    serde_json::from_slice::<serde_json::Value>(bytes).unwrap()["meta"].clone()
                })
                .collect();
            assert!(metas.windows(2).all(|pair| pair[0] == pair[1]), "{metas:?}");
            let as_of = metas[0]["fetchedAt"].as_i64().unwrap();
            assert!(as_of < chrono::Utc::now().timestamp() - 3600, "{as_of}");
            let growth: serde_json::Value = serde_json::from_slice(&first[2].1).unwrap();
            assert_eq!(growth["windowEnd"].as_i64(), Some(as_of));
            assert!(growth.get("status").is_none());

            // Recomputed at a later wall time (past every TTL): the same
            // version yields the same bytes and the same tags.
            tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
            for (name, bytes) in &first {
                let again = get_with(&router, &part(name, epoch), &[]).await;
                assert_eq!(&body(again).await, bytes, "{name}");
            }

            // The wall-clock overview never claims a version.
            let overview = get_with(
                &router,
                &format!("{base}/overview?interval=60&v={epoch}"),
                &gzip,
            )
            .await;
            assert_eq!(overview.status(), StatusCode::OK);
            assert_eq!(header(&overview, "cache-control"), Some(LIVE_CACHE_CONTROL));
            // Nor does the live status.
            let status = get_with(&router, &format!("{base}/status?v={epoch}"), &gzip).await;
            assert_eq!(status.status(), StatusCode::OK);
            assert_eq!(header(&status, "cache-control"), Some(LIVE_CACHE_CONTROL));

            // No samples yet (World Link chapter 99 has none): the read is
            // not pinned to a cut, so even the current `v` stays short-lived.
            let unpinned = get_with(
                &router,
                &format!(
                    "/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/99/top100?interval=60&v={wb_epoch}"
                ),
                &gzip,
            )
            .await;
            assert_eq!(unpinned.status(), StatusCode::OK);
            assert_eq!(header(&unpinned, "cache-control"), Some(LIVE_CACHE_CONTROL));
            let pinned = get_with(
                &router,
                &format!(
                    "/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/top100?interval=60&v={wb_epoch}"
                ),
                &gzip,
            )
            .await;
            assert_eq!(
                header(&pinned, "cache-control"),
                Some(VERSIONED_CACHE_CONTROL)
            );

            let stale = get_with(&router, &part("top100", epoch - 1), &gzip).await;
            assert_eq!(header(&stale, "cache-control"), Some(LIVE_CACHE_CONTROL));

            // After the next write the old version is no longer vouched for.
            let next = finish_event_update(&mut conn, SekaiServerRegion::Jp, NORMAL_EVENT)
                .await
                .unwrap();
            assert_eq!(next, epoch + 1);
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let old = get_with(&router, &part("top100", epoch), &gzip).await;
            assert_eq!(header(&old, "cache-control"), Some(LIVE_CACHE_CONTROL));
            let new = get_with(&router, &part("top100", next), &gzip).await;
            assert_eq!(header(&new, "cache-control"), Some(VERSIONED_CACHE_CONTROL));
        });
    }
}
