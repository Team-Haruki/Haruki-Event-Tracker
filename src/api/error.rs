//! Handler-level error type. Maps to one of three Go responses:
//! 400 (bad path/server), 404 (no row), 500 (db error). Body is always
//! `{"error": "..."}` to match the fiber JSON shape.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sea_orm::DbErr;
use serde::Serialize;

use crate::api::json::Json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("invalid server: {0}")]
    InvalidServer(String),
    #[error("not found")]
    NotFound,
    #[error("invalid path parameter: {0}")]
    BadRequest(String),
    #[error("database: {0}")]
    Db(#[from] DbErr),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Db(err) = &self {
            tracing::error!(%err, "db error in handler");
        }
        let status = match &self {
            ApiError::InvalidServer(_) | ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(ErrorBody {
            error: self.to_string(),
        });
        (status, body).into_response()
    }
}
