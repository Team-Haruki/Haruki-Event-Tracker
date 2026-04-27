//! Mounts the 14 routes from `api/api.go::RegisterRoutes` onto an Axum
//! router. The middleware stack mirrors the Go fiber app: panic catcher
//! → compression (gzip+brotli) → access log.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;

use crate::api::access_log::{self, ProxyTrust};
use crate::api::handler::{lines, ranking, status, trace, user, world_bloom};
use crate::api::state::AppState;

pub fn build_router(state: AppState, trust: Arc<ProxyTrust>) -> Router {
    let event_routes = Router::new()
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
        .route(
            "/trace-world-bloom-ranking/character/{character_id}/user/{user_id}",
            get(trace::wb_all_by_user),
        )
        .route(
            "/trace-world-bloom-ranking/character/{character_id}/rank/{rank}",
            get(trace::wb_all_by_rank),
        )
        .route("/user-data/{user_id}", get(user::user_data))
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
        .route("/status", get(status::event_status));

    Router::new()
        .nest("/event/{server}/{event_id}", event_routes)
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(trust, access_log::log))
        .layer(CompressionLayer::new().gzip(true).br(true))
        .layer(CatchPanicLayer::new())
}
