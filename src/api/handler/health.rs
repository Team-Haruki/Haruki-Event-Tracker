use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::api::json::Json;
use crate::api::state::AppState;

#[derive(Serialize)]
pub struct LiveResponse {
    status: &'static str,
}

#[derive(Serialize)]
pub struct ReadyResponse {
    status: &'static str,
    databases: Vec<DatabaseStatus>,
}

#[derive(Serialize)]
pub struct DatabaseStatus {
    server: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn livez() -> Json<LiveResponse> {
    Json(LiveResponse { status: "ok" })
}

pub async fn readyz(State(state): State<AppState>) -> Response {
    let mut databases = Vec::new();
    let mut ready = true;

    for (server, db) in state.dbs() {
        match db.ping().await {
            Ok(()) => databases.push(DatabaseStatus {
                server: server.to_string(),
                status: "ok",
                error: None,
            }),
            Err(err) => {
                ready = false;
                databases.push(DatabaseStatus {
                    server: server.to_string(),
                    status: "error",
                    error: Some(err.to_string()),
                });
            }
        }
    }

    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = Json(ReadyResponse {
        status: if ready { "ok" } else { "error" },
        databases,
    });
    (status, body).into_response()
}
