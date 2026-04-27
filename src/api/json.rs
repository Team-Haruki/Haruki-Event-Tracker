//! `Json<T>` response wrapper backed by sonic-rs.
//!
//! All API endpoints are GET, so we only need `IntoResponse` (no
//! `FromRequest`). Mirrors fiber's `c.JSON(...)` shape: serialized body
//! with `Content-Type: application/json`.

use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

pub struct Json<T>(pub T);

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        match sonic_rs::to_vec(&self.0) {
            Ok(bytes) => (
                [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
                bytes,
            )
                .into_response(),
            Err(err) => {
                tracing::error!(?err, "json encode error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
                    br#"{"error":"json encode error"}"#.as_slice(),
                )
                    .into_response()
            }
        }
    }
}
