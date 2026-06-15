//! Mounts the 14 routes from `api/api.go::RegisterRoutes` onto an Axum
//! router. The middleware stack mirrors the Go fiber app: panic catcher
//! → compression (gzip+brotli) → access log.

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::get;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;

use crate::api::access_log::{self, ProxyTrust};
use crate::api::handler::{health, lines, private, ranking, status, trace, user, web, world_bloom};
use crate::api::state::AppState;
use crate::api::{ws, ws_ticket};

pub fn build_router(state: AppState, trust: Arc<ProxyTrust>) -> Router {
    let event_routes = event_routes();
    let private_routes = private_event_routes().route_layer(middleware::from_fn_with_state(
        trust.clone(),
        private::require_subject,
    ));
    let ws_state = (state.clone(), trust.clone());

    Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .route(
            "/ws-ticket",
            get(ws_ticket::issue_ticket).with_state(ws_state.clone()),
        )
        .route("/ws", get(ws::connect).with_state(ws_state))
        .nest("/event/{server}/{event_id}/private", private_routes)
        .nest("/event/{server}/{event_id}", event_routes)
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(trust, access_log::log))
        .layer(CompressionLayer::new().gzip(true).br(true))
        .layer(CatchPanicLayer::new())
}

pub fn private_event_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/latest-ranking/user/{user_id}",
            get(private::latest_by_user),
        )
        .route(
            "/latest-world-bloom-ranking/character/{character_id}/user/{user_id}",
            get(private::latest_world_bloom_by_user),
        )
        .route("/trace-ranking/user/{user_id}", get(private::trace_by_user))
        .route(
            "/trace-world-bloom-ranking/character/{character_id}/user/{user_id}",
            get(private::trace_world_bloom_by_user),
        )
}

pub fn event_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/latest-ranking/user/{user_id}",
            get(ranking::latest_by_user),
        )
        .route("/latest-ranking/rank/{rank}", get(ranking::latest_by_rank))
        .route(
            "/latest-world-bloom-ranking/character/{character_id}/user/{user_id}",
            get(world_bloom::latest_by_user),
        )
        .route(
            "/latest-world-bloom-ranking/character/{character_id}/rank/{rank}",
            get(world_bloom::latest_by_rank),
        )
        .route("/trace-ranking/user/{user_id}", get(trace::all_by_user))
        .route("/trace-ranking/rank/{rank}", get(trace::all_by_rank))
        .route("/trace-ranking/ranks", get(trace::all_by_ranks))
        .route(
            "/trace-world-bloom-ranking/character/{character_id}/user/{user_id}",
            get(trace::wb_all_by_user),
        )
        .route(
            "/trace-world-bloom-ranking/character/{character_id}/rank/{rank}",
            get(trace::wb_all_by_rank),
        )
        .route(
            "/trace-world-bloom-ranking/character/{character_id}/ranks",
            get(trace::wb_all_by_ranks),
        )
        .route("/user-data/{user_id}", get(user::user_data))
        .route("/web/rankings", get(web::rankings))
        .route(
            "/web/world-bloom-rankings/character/{character_id}",
            get(web::world_bloom_rankings),
        )
        .route("/web/trace-ranking/user/{user_id}", get(web::user_trace))
        .route(
            "/web/trace-world-bloom-ranking/character/{character_id}/user/{user_id}",
            get(web::world_bloom_user_trace),
        )
        .route("/web/users", get(web::users))
        .route("/ranking-lines", get(lines::ranking_lines))
        .route(
            "/ranking-score-growth/interval/{interval}",
            get(lines::score_growth),
        )
        .route(
            "/world-bloom-ranking-lines/character/{character_id}",
            get(lines::wb_ranking_lines),
        )
        .route(
            "/world-bloom-ranking-score-growth/character/{character_id}/interval/{interval}",
            get(lines::wb_score_growth),
        )
        .route("/status", get(status::event_status))
}
