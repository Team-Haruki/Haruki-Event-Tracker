//! Mounts the public Tracker API routes. Middleware, outermost first: panic
//! catcher → access log → web HTTP caching (ETag / 304 / Cache-Control on
//! `/api/v2/web/`) → compression (gzip, brotli, zstd).

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use tower_http::CompressionLevel;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{And, DefaultPredicate, Predicate, SizeAbove};

use crate::api::access_log::{self, ProxyTrust};
use crate::api::handler::{health, leaderboard, private, status, web};
use crate::api::state::AppState;
use crate::api::{cloud_auth, cluster, http_cache, ws, ws_ticket};

/// A cluster `writer` exposes only health plus the update stream; every
/// other role serves the public surface (cloud group behind the optional
/// bearer token, web group, WebSocket realtime).
pub fn build_router(state: AppState, trust: Arc<ProxyTrust>) -> Router {
    let ws_state = (state.clone(), trust.clone());
    let mut base = Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz));
    // Any process that runs tracker daemons can take the registry webhook;
    // the bearer check inside rejects everything when no token is set.
    if !state.cluster_token().is_empty() && !state.role().is_reader() {
        base = base.route("/internal/master-updated", post(cluster::master_updated));
    }
    let router = if state.role().is_writer() {
        base.route("/internal/updates", get(cluster::updates))
    } else {
        base.route(
            "/ws-ticket",
            get(ws_ticket::issue_ticket).with_state(ws_state.clone()),
        )
        .route("/ws", get(ws::connect).with_state(ws_state))
        .merge(
            cloud_v2_routes().route_layer(middleware::from_fn_with_state(
                state.clone(),
                cloud_auth::require_cloud_token,
            )),
        )
        .merge(web_v2_routes(trust.clone()))
    };

    router
        .with_state(state)
        .layer(compression_layer())
        // Outside compression so ETags and 304s cover the encoded bytes;
        // inside the access log so a 304 is logged as one.
        .layer(middleware::from_fn(http_cache::web_cache_headers))
        .layer(axum::middleware::from_fn_with_state(trust, access_log::log))
        .layer(CatchPanicLayer::new())
}

/// Response compression for the public surface. Bodies already carrying a
/// `Content-Encoding` (the precompressed cached overviews) pass through
/// untouched.
///
/// Level 1 everywhere: without an explicit quality tower-http hands brotli
/// its library default (quality 11, ~1 MB/s), and even level 4 spent 2–3 ms
/// per large detail inline on a worker. On this data zstd 1 and br 1 are
/// both smaller and 2–5x faster than gzip 4; gzip 1 is ~19 % larger than
/// gzip 4, which only reaches clients without br/zstd support.
pub fn compression_layer() -> CompressionLayer<And<SizeAbove, DefaultPredicate>> {
    CompressionLayer::new()
        .gzip(true)
        .br(true)
        .zstd(true)
        .quality(CompressionLevel::Precise(1))
        .compress_when(SizeAbove::new(1024).and(DefaultPredicate::new()))
}

pub fn cloud_v2_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/query",
            get(leaderboard::cloud::total_query),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/check-room",
            get(leaderboard::cloud::total_check_room),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/line",
            get(leaderboard::cloud::total_line),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/speed",
            get(leaderboard::cloud::total_speed),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/trace",
            get(leaderboard::cloud::total_trace),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/status",
            get(status::event_status),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/sk/query",
            get(leaderboard::cloud::world_bloom_query),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/sk/check-room",
            get(leaderboard::cloud::world_bloom_check_room),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/sk/line",
            get(leaderboard::cloud::world_bloom_line),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/sk/speed",
            get(leaderboard::cloud::world_bloom_speed),
        )
        .route(
            "/api/v2/cloud/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/sk/trace",
            get(leaderboard::cloud::world_bloom_trace),
        )
}

pub fn web_v2_routes(trust: Arc<ProxyTrust>) -> Router<AppState> {
    let private_routes = Router::new()
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/private/details/user/{user_id}",
            get(private::web_total_user_detail),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/private/details/user/{user_id}",
            get(private::web_world_bloom_user_detail),
        )
        .route_layer(middleware::from_fn_with_state(trust, private::require_subject));

    Router::new()
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/overview",
            get(leaderboard::web::total_overview),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/replay/overview",
            get(leaderboard::web::total_replay_overview),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/top100",
            get(leaderboard::web::total_top100),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/borders",
            get(leaderboard::web::total_borders),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/growth",
            get(leaderboard::web::total_growth),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/status",
            get(leaderboard::web::total_status),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/details/rank/{rank}",
            get(leaderboard::web::total_rank_detail),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/details/user/{user_id}",
            get(leaderboard::web::total_user_detail),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/check-room",
            get(leaderboard::web::total_check_room),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/users/search",
            get(web::users),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/overview",
            get(leaderboard::web::world_bloom_overview),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/replay/overview",
            get(leaderboard::web::world_bloom_replay_overview),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/top100",
            get(leaderboard::web::world_bloom_top100),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/borders",
            get(leaderboard::web::world_bloom_borders),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/growth",
            get(leaderboard::web::world_bloom_growth),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/status",
            get(leaderboard::web::world_bloom_status),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/details/rank/{rank}",
            get(leaderboard::web::world_bloom_rank_detail),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/details/user/{user_id}",
            get(leaderboard::web::world_bloom_user_detail),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/check-room",
            get(leaderboard::web::world_bloom_check_room),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/users/search",
            get(leaderboard::web::world_bloom_users),
        )
        .merge(private_routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handler::web::tests::{NORMAL_EVENT, WORLD_BLOOM_EVENT, test_state};
    use crate::api::state::ClusterState;
    use crate::cluster::UpdateBus;
    use crate::config::ClusterRole;
    use crate::model::enums::SekaiServerRegion;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn trust() -> Arc<ProxyTrust> {
        let (trust, invalid) = ProxyTrust::from_config(false, &[], "X-Forwarded-For", 1.0, 1000);
        assert!(invalid.is_empty());
        Arc::new(trust)
    }

    async fn status(router: &Router, uri: &str) -> StatusCode {
        router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    async fn status_with_bearer(router: &Router, uri: &str, token: &str) -> StatusCode {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[test]
    fn cloud_group_requires_a_token_only_when_configured() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let state = test_state(true).await.with_cluster(ClusterState {
                            cloud_tokens: vec!["alpha".into(), "beta".into()],
                            ..ClusterState::default()
                        });
                        let router = build_router(state, trust());
                        let cloud = format!(
                            "/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/query?rank=1"
                        );
                        let web = format!(
                            "/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/overview?at=1710000060&interval=60"
                        );
                        assert_eq!(status(&router, &cloud).await, StatusCode::UNAUTHORIZED);
                        assert_eq!(
                            status_with_bearer(&router, &cloud, "nope").await,
                            StatusCode::UNAUTHORIZED
                        );
                        assert_eq!(status_with_bearer(&router, &cloud, "beta").await, StatusCode::OK);
                        // The web group is unaffected by cloud tokens.
                        assert_eq!(status(&router, &web).await, StatusCode::OK);
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn get(router: &Router, uri: &str) -> axum::response::Response {
        router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[test]
    fn details_answer_unranked_players_and_empty_cursor_polls_with_200() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let state = test_state(true).await;
                        let (_, engine) =
                            crate::api::extract::resolve_region_engine(&state, "jp").unwrap();
                        crate::db::query::web::tests::seed_player_pushed_out(
                            &engine,
                            NORMAL_EVENT,
                            None,
                            3,
                        )
                        .await;
                        let gamma = state.anonymizer().public_user_id(
                            SekaiServerRegion::Jp,
                            NORMAL_EVENT,
                            "300",
                        );
                        let never_seen = state.anonymizer().public_user_id(
                            SekaiServerRegion::Jp,
                            NORMAL_EVENT,
                            "999",
                        );
                        let router = build_router(state, trust());
                        let web = format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total");

                        // Per-request details are never immutable, even with `v`.
                        for path in [
                            format!("{web}/details/user/{gamma}?includeTrace=true&v=7"),
                            format!("{web}/details/rank/3?includeTrace=true&includePlayerTrace=false&cursor=1710000120&limit=5000&v=7"),
                            format!("{web}/details/user/{gamma}?includeTrace=true&cursor=1710000120"),
                        ] {
                            let response = get(&router, &path).await;
                            assert_eq!(response.status(), StatusCode::OK, "{path}");
                            assert_eq!(
                                response.headers()[axum::http::header::CACHE_CONTROL],
                                crate::api::http_cache::LIVE_CACHE_CONTROL,
                                "{path}"
                            );
                        }
                        let body = axum::body::to_bytes(
                            get(&router, &format!("{web}/details/user/{gamma}?includeTrace=true"))
                                .await
                                .into_body(),
                            usize::MAX,
                        )
                        .await
                        .unwrap();
                        let body = std::str::from_utf8(&body).unwrap();
                        assert!(body.contains("\"ranked\":false"), "{body}");
                        assert!(!body.contains("\"current\""), "{body}");

                        assert_eq!(
                            status(&router, &format!("{web}/details/user/{never_seen}")).await,
                            StatusCode::NOT_FOUND
                        );
                        // Cloud keeps "not in the tracked ranks" as a 404.
                        assert_eq!(
                            status(
                                &router,
                                &format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/query?userId=300"),
                            )
                            .await,
                            StatusCode::NOT_FOUND
                        );
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[tokio::test]
    async fn compression_negotiates_zstd_br_gzip_and_skips_precompressed_bodies() {
        use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
        use tokio::io::AsyncReadExt;

        let plain =
            serde_json::to_string(&(0..400).map(|i| (i, "x".repeat(8))).collect::<Vec<_>>())
                .unwrap();
        let gzipped = {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(plain.as_bytes()).unwrap();
            encoder.finish().unwrap()
        };
        let plain_for_route = plain.clone();
        let gzipped_for_route = gzipped.clone();
        let app = Router::new()
            .route(
                "/api/v2/web/plain",
                axum::routing::get(move || {
                    let body = plain_for_route.clone();
                    async move {
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            body,
                        )
                    }
                }),
            )
            .route(
                "/api/v2/web/precompressed",
                axum::routing::get(move || {
                    let body = gzipped_for_route.clone();
                    async move {
                        (
                            [
                                (axum::http::header::CONTENT_TYPE, "application/json"),
                                (CONTENT_ENCODING, "gzip"),
                            ],
                            body,
                        )
                    }
                }),
            )
            .layer(compression_layer());

        async fn fetch(app: &Router, uri: &str, accept: Option<&str>) -> (Option<String>, Vec<u8>) {
            let mut request = Request::builder().uri(uri);
            if let Some(accept) = accept {
                request = request.header(ACCEPT_ENCODING, accept);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let encoding = response
                .headers()
                .get(CONTENT_ENCODING)
                .map(|value| value.to_str().unwrap().to_owned());
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (encoding, body.to_vec())
        }

        async fn decode(encoding: Option<&str>, body: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            match encoding {
                None => out.extend_from_slice(body),
                Some("gzip") => {
                    async_compression::tokio::bufread::GzipDecoder::new(body)
                        .read_to_end(&mut out)
                        .await
                        .unwrap();
                }
                Some("br") => {
                    async_compression::tokio::bufread::BrotliDecoder::new(body)
                        .read_to_end(&mut out)
                        .await
                        .unwrap();
                }
                Some("zstd") => {
                    async_compression::tokio::bufread::ZstdDecoder::new(body)
                        .read_to_end(&mut out)
                        .await
                        .unwrap();
                }
                Some(other) => panic!("unexpected encoding {other}"),
            }
            out
        }

        for (accept, expected) in [
            (Some("gzip, deflate, br, zstd"), Some("zstd")),
            (Some("br"), Some("br")),
            (Some("gzip"), Some("gzip")),
            (Some("identity"), None),
            (None, None),
        ] {
            let (encoding, body) = fetch(&app, "/api/v2/web/plain", accept).await;
            assert_eq!(encoding.as_deref(), expected, "accept {accept:?}");
            if expected.is_some() {
                assert!(body.len() < plain.len(), "accept {accept:?} did not shrink");
            }
            let decoded = decode(encoding.as_deref(), &body).await;
            assert_eq!(decoded, plain.as_bytes(), "accept {accept:?}");
        }

        // A body that arrives with its own Content-Encoding is never
        // re-encoded, whatever the client accepts.
        for accept in [Some("gzip, br, zstd"), Some("zstd"), None] {
            let (encoding, body) = fetch(&app, "/api/v2/web/precompressed", accept).await;
            assert_eq!(encoding.as_deref(), Some("gzip"), "accept {accept:?}");
            assert_eq!(body, gzipped, "accept {accept:?}");
        }
    }

    #[tokio::test]
    async fn writer_role_exposes_only_health_and_the_update_stream() {
        let state = test_state(true).await.with_cluster(ClusterState {
            role: ClusterRole::Writer,
            cluster_token: "secret".into(),
            update_bus: Some(UpdateBus::new()),
            ..ClusterState::default()
        });
        let router = build_router(state, trust());
        assert_eq!(status(&router, "/livez").await, StatusCode::OK);
        assert_eq!(status(&router, "/readyz").await, StatusCode::OK);
        let cloud =
            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/query?rank=1");
        assert_eq!(status(&router, &cloud).await, StatusCode::NOT_FOUND);
        assert_eq!(status(&router, "/ws-ticket").await, StatusCode::NOT_FOUND);
        // Mounted, but a plain GET is not a WebSocket handshake.
        assert_ne!(
            status(&router, "/internal/updates").await,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn routes_all_cloud_and_web_endpoints() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let state = test_state(true).await;
                        let user = state.anonymizer().public_user_id(
                            SekaiServerRegion::Jp,
                            NORMAL_EVENT,
                            "100",
                        );
                        let world_user = state.anonymizer().public_user_id(
                            SekaiServerRegion::Jp,
                            WORLD_BLOOM_EVENT,
                            "100",
                        );
                        let router = build_router(state, trust());

                        assert_eq!(status(&router, "/livez").await, StatusCode::OK);
                        assert_eq!(status(&router, "/readyz").await, StatusCode::OK);
                        assert_eq!(status(&router, "/missing").await, StatusCode::NOT_FOUND);

                        let cloud_paths = [
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/query?rank=1"),
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/check-room?rank=1"),
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/line?rank=1"),
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/speed?rank=1&interval=60"),
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/trace?subject=100"),
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/check-room?userId=100"),
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/status"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/query?rank=1"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/check-room?rank=1"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/line?rank=1"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/speed?rank=1&interval=60"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/trace?subject=100"),
                        ];
                        for path in cloud_paths {
                            assert_eq!(status(&router, &path).await, StatusCode::OK, "{path}");
                        }

                        let web_paths = [
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/replay/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/top100?at=1710000060"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/borders"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/growth?interval=60&v=3"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/top100"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/borders?at=1710000060"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/growth?interval=60"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/status"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/status?at=1710000060"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/details/rank/1?at=1710000060"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/details/user/{user}"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/users/search?name=Alpha"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/check-room?userId=100"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/details/user/100?idType=uid"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/check-room?userId=100"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/replay/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/details/rank/1?at=1710000060"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/details/user/{world_user}"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/users/search?name=Alpha"),
                        ];
                        for path in web_paths {
                            assert_eq!(status(&router, &path).await, StatusCode::OK, "{path}");
                        }

                        // Cloud speaks raw UIDs even though anonymization is on;
                        // a unique_id is not a valid cloud subject.
                        let cloud_unique = format!(
                            "/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/trace?subject={user}"
                        );
                        assert_eq!(status(&router, &cloud_unique).await, StatusCode::NOT_FOUND);
                        // A bare numeric id is a game UID lookup (revealed for
                        // that player); forcing `idType=unique` on it is a miss.
                        let web_raw = format!(
                            "/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/details/user/100"
                        );
                        assert_eq!(status(&router, &web_raw).await, StatusCode::OK);
                        let web_forced_unique = format!("{web_raw}?idType=unique");
                        assert_eq!(status(&router, &web_forced_unique).await, StatusCode::NOT_FOUND);
                        let _ = world_user;

                        let private = format!(
                            "/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/private/details/user/100"
                        );
                        assert_eq!(status(&router, &private).await, StatusCode::UNAUTHORIZED);
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
