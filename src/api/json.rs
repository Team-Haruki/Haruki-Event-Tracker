//! `Json<T>` response wrapper backed by sonic-rs.
//!
//! All API endpoints are GET, so we only need `IntoResponse` (no
//! `FromRequest`). Mirrors fiber's `c.JSON(...)` shape: serialized body
//! with `Content-Type: application/json`.

use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;

pub struct Json<T>(pub T);
pub struct RawJson(pub Bytes);

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

impl IntoResponse for RawJson {
    fn into_response(self) -> Response {
        (
            [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
            self.0,
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header::CONTENT_TYPE;

    #[tokio::test]
    async fn raw_json_returns_exact_bytes_and_content_type() {
        let response = RawJson(Bytes::from_static(br#"{"ok":true}"#)).into_response();
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/json")
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, Bytes::from_static(br#"{"ok":true}"#));
    }
}
