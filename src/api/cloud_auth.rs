//! Bearer-token gate for `/api/v2/cloud/*`. Enforced only when
//! `cloud_api.tokens` is non-empty, so single-node deployments that predate
//! the cluster keep working unchanged (with a startup warning).

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::json::Json;
use crate::api::state::AppState;
use crate::cluster::{bearer_token, token_matches};

#[derive(serde::Serialize)]
struct ErrorBody {
    error: &'static str,
}

pub async fn require_cloud_token(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let tokens = state.cloud_tokens();
    if tokens.is_empty() {
        return next.run(req).await;
    }
    let presented = bearer_token(req.headers());
    // Compare against every token so timing does not reveal which slot
    // (if any) matched.
    let mut ok = false;
    if let Some(presented) = presented {
        for token in tokens {
            ok |= token_matches(presented, token);
        }
    }
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                error: "cloud api token required",
            }),
        )
            .into_response();
    }
    next.run(req).await
}
