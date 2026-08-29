//! Mounts the public Tracker API routes. The middleware stack mirrors the Go
//! fiber app: panic catcher → compression (gzip+brotli) → access log.

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::get;
use tower_http::CompressionLevel;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, Predicate, SizeAbove};

use crate::api::access_log::{self, ProxyTrust};
use crate::api::handler::{health, leaderboard, private, status, web};
use crate::api::state::AppState;
use crate::api::{ws, ws_ticket};

pub fn build_router(state: AppState, trust: Arc<ProxyTrust>) -> Router {
    let ws_state = (state.clone(), trust.clone());

    Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .route(
            "/ws-ticket",
            get(ws_ticket::issue_ticket).with_state(ws_state.clone()),
        )
        .route("/ws", get(ws::connect).with_state(ws_state))
        .merge(cloud_v2_routes())
        .merge(web_v2_routes(trust.clone()))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(trust, access_log::log))
        // Without an explicit quality tower-http hands brotli its library
        // default (quality 11, ~1 MB/s); browsers prefer br over gzip, so
        // every large response would eat that cost inline on a worker.
        .layer(
            CompressionLayer::new()
                .gzip(true)
                .br(true)
                .quality(CompressionLevel::Precise(4))
                .compress_when(SizeAbove::new(1024).and(DefaultPredicate::new())),
        )
        .layer(CatchPanicLayer::new())
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
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/details/rank/{rank}",
            get(leaderboard::web::total_rank_detail),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/total/details/user/{user_id}",
            get(leaderboard::web::total_user_detail),
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
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/details/rank/{rank}",
            get(leaderboard::web::world_bloom_rank_detail),
        )
        .route(
            "/api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/details/user/{user_id}",
            get(leaderboard::web::world_bloom_user_detail),
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
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/trace?subject={user}"),
                            format!("/api/v2/cloud/events/jp/{NORMAL_EVENT}/leaderboards/total/sk/status"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/query?rank=1"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/check-room?rank=1"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/line?rank=1"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/speed?rank=1&interval=60"),
                            format!("/api/v2/cloud/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/sk/trace?subject={world_user}"),
                        ];
                        for path in cloud_paths {
                            assert_eq!(status(&router, &path).await, StatusCode::OK, "{path}");
                        }

                        let web_paths = [
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/replay/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/details/rank/1?at=1710000060"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/details/user/{user}"),
                            format!("/api/v2/web/events/jp/{NORMAL_EVENT}/leaderboards/total/users/search?name=Alpha"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/replay/overview?at=1710000060&interval=60"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/details/rank/1?at=1710000060"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/details/user/{world_user}"),
                            format!("/api/v2/web/events/jp/{WORLD_BLOOM_EVENT}/leaderboards/world-bloom/17/users/search?name=Alpha"),
                        ];
                        for path in web_paths {
                            assert_eq!(status(&router, &path).await, StatusCode::OK, "{path}");
                        }

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
