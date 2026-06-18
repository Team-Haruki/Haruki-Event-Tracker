//! Mounts the public Tracker API routes. The middleware stack mirrors the Go
//! fiber app: panic catcher → compression (gzip+brotli) → access log.

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::get;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;

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
        .layer(CompressionLayer::new().gzip(true).br(true))
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
