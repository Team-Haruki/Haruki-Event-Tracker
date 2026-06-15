//! `Json<T>` response wrapper backed by sonic-rs.
//!
//! All API endpoints are GET, so we only need `IntoResponse` (no
//! `FromRequest`). Mirrors fiber's `c.JSON(...)` shape: serialized body
//! with `Content-Type: application/json`.

use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE, VARY};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;

pub struct Json<T>(pub T);
pub struct RawJson(pub Bytes);
pub struct EncodedJson {
    bytes: Bytes,
    encoding: JsonEncoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonEncoding {
    Identity,
    Gzip,
}

impl EncodedJson {
    pub fn identity(bytes: Bytes) -> Self {
        Self {
            bytes,
            encoding: JsonEncoding::Identity,
        }
    }

    pub fn gzip(bytes: Bytes) -> Self {
        Self {
            bytes,
            encoding: JsonEncoding::Gzip,
        }
    }
}

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

impl IntoResponse for EncodedJson {
    fn into_response(self) -> Response {
        match self.encoding {
            JsonEncoding::Identity => (
                [
                    (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                    (VARY, HeaderValue::from_static("accept-encoding")),
                ],
                self.bytes,
            )
                .into_response(),
            JsonEncoding::Gzip => (
                [
                    (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                    (CONTENT_ENCODING, HeaderValue::from_static("gzip")),
                    (VARY, HeaderValue::from_static("accept-encoding")),
                ],
                self.bytes,
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE, VARY};

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

    #[tokio::test]
    async fn encoded_json_identity_returns_exact_bytes() {
        let response = EncodedJson::identity(Bytes::from_static(br#"{"ok":true}"#)).into_response();
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/json")
        );
        assert_eq!(
            response.headers().get(VARY).unwrap(),
            HeaderValue::from_static("accept-encoding")
        );
        assert!(response.headers().get(CONTENT_ENCODING).is_none());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, Bytes::from_static(br#"{"ok":true}"#));
    }

    #[tokio::test]
    async fn encoded_json_gzip_sets_content_encoding() {
        let response = EncodedJson::gzip(Bytes::from_static(b"gzipped")).into_response();
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/json")
        );
        assert_eq!(
            response.headers().get(CONTENT_ENCODING).unwrap(),
            HeaderValue::from_static("gzip")
        );
        assert_eq!(
            response.headers().get(VARY).unwrap(),
            HeaderValue::from_static("accept-encoding")
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, Bytes::from_static(b"gzipped"));
    }
}
