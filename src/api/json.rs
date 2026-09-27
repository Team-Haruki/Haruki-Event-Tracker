//! `Json<T>` response wrapper backed by sonic-rs.
//!
//! All API endpoints are GET, so we only need `IntoResponse` (no
//! `FromRequest`). Mirrors fiber's `c.JSON(...)` shape: serialized body
//! with `Content-Type: application/json`.

use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, VARY};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;

use crate::api::http_cache::{PrecomputedEtag, ServedEpoch};

pub struct Json<T>(pub T);
pub struct RawJson(pub Bytes);
pub struct EncodedJson {
    bytes: Bytes,
    encoding: JsonEncoding,
    epoch: Option<i64>,
    etag: Option<HeaderValue>,
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
            epoch: None,
            etag: None,
        }
    }

    pub fn gzip(bytes: Bytes) -> Self {
        Self {
            bytes,
            encoding: JsonEncoding::Gzip,
            epoch: None,
            etag: None,
        }
    }

    /// The plain JSON bytes, or `None` for a gzip body.
    pub fn into_identity_bytes(self) -> Option<Bytes> {
        (self.encoding == JsonEncoding::Identity).then_some(self.bytes)
    }

    /// Tags the response with the API-cache epoch its bytes belong to
    /// (`ServedEpoch` extension), which lets `http_cache` mark a matching
    /// `v=<epoch>` request immutable.
    pub fn at_epoch(mut self, epoch: Option<i64>) -> Self {
        self.epoch = epoch;
        self
    }

    /// Carries the strong ETag of these exact bytes, digested when the
    /// cache produced them (`PrecomputedEtag` extension), so `http_cache`
    /// can skip hashing the body per request.
    pub fn with_etag(mut self, etag: Option<HeaderValue>) -> Self {
        self.etag = etag;
        self
    }
}

/// Whether the request's `Accept-Encoding` admits gzip (`gzip` or `*`,
/// not disabled with `q=0`). Used to pick the precompressed cache variant.
pub fn accepts_gzip(headers: &axum::http::HeaderMap) -> bool {
    let Some(value) = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    value.split(',').any(|part| {
        let mut params = part.trim().split(';');
        let coding = params.next().unwrap_or("").trim();
        if !coding.eq_ignore_ascii_case("gzip") && coding != "*" {
            return false;
        }
        !params.any(|p| {
            let p = p.trim();
            p.eq_ignore_ascii_case("q=0")
                || p.eq_ignore_ascii_case("q=0.0")
                || p.eq_ignore_ascii_case("q=0.00")
                || p.eq_ignore_ascii_case("q=0.000")
        })
    })
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
        let mut response = self.encoding_response();
        if let Some(epoch) = self.epoch {
            response.extensions_mut().insert(ServedEpoch(epoch));
        }
        if let Some(etag) = self.etag {
            let content_encoding = match self.encoding {
                JsonEncoding::Identity => None,
                JsonEncoding::Gzip => Some(HeaderValue::from_static("gzip")),
            };
            response.extensions_mut().insert(PrecomputedEtag {
                etag,
                content_encoding,
            });
        }
        response
    }
}

impl EncodedJson {
    fn encoding_response(&self) -> Response {
        let bytes = self.bytes.clone();
        match self.encoding {
            JsonEncoding::Identity => (
                [
                    (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                    (VARY, HeaderValue::from_static("accept-encoding")),
                ],
                bytes,
            )
                .into_response(),
            JsonEncoding::Gzip => (
                [
                    (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                    (CONTENT_ENCODING, HeaderValue::from_static("gzip")),
                    (VARY, HeaderValue::from_static("accept-encoding")),
                ],
                bytes,
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
    use serde::Serializer;

    struct BadJson;

    impl Serialize for BadJson {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom("expected failure"))
        }
    }

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
    async fn encoded_json_carries_its_precomputed_etag_with_the_encoding() {
        let etag = HeaderValue::from_static("\"abc\"");
        let gzip = EncodedJson::gzip(Bytes::from_static(b"gzipped"))
            .with_etag(Some(etag.clone()))
            .into_response();
        let precomputed = gzip.extensions().get::<PrecomputedEtag>().unwrap();
        assert_eq!(precomputed.etag, etag);
        assert_eq!(
            precomputed.content_encoding,
            Some(HeaderValue::from_static("gzip"))
        );

        let identity = EncodedJson::identity(Bytes::from_static(b"{}"))
            .with_etag(Some(etag.clone()))
            .into_response();
        let precomputed = identity.extensions().get::<PrecomputedEtag>().unwrap();
        assert_eq!(precomputed.etag, etag);
        assert_eq!(precomputed.content_encoding, None);

        let untagged = EncodedJson::gzip(Bytes::from_static(b"gzipped")).into_response();
        assert!(untagged.extensions().get::<PrecomputedEtag>().is_none());
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

    #[test]
    fn accepts_gzip_honors_wildcards_case_and_zero_quality() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!accepts_gzip(&headers));
        for value in ["gzip", "br, GZIP; q=1", "*"] {
            headers.insert(ACCEPT_ENCODING, value.parse().unwrap());
            assert!(accepts_gzip(&headers), "{value}");
        }
        for value in [
            "br",
            "gzip;q=0",
            "gzip;q=0.0",
            "gzip;q=0.00",
            "gzip;q=0.000",
        ] {
            headers.insert(ACCEPT_ENCODING, value.parse().unwrap());
            assert!(!accepts_gzip(&headers), "{value}");
        }
    }

    #[tokio::test]
    async fn json_wrapper_reports_serialization_errors() {
        let response = Json(BadJson).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            body,
            Bytes::from_static(br#"{"error":"json encode error"}"#)
        );
    }
}
