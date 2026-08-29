//! `GetTop100` / `GetBorder` ports. Returns the parsed body and — for
//! border, where the tracker uses it as a cache key — the raw SHA-256 of
//! the response bytes (Go: `tracker/callapi.go:54`).

use sha2::{Digest, Sha256};

use crate::model::enums::SekaiServerRegion;
use crate::model::sekai::{BorderRankingResponse, Top100RankingResponse};
use crate::sekai_api::client::HarukiSekaiAPIClient;
use crate::sekai_api::error::SekaiApiError;

impl HarukiSekaiAPIClient {
    fn url(&self, server: SekaiServerRegion, event_id: i64, leaf: &str) -> String {
        format!(
            "{}/{}/event/{}/{}",
            self.api_endpoint, server, event_id, leaf
        )
    }

    #[tracing::instrument(skip(self), fields(server = %server, event_id))]
    pub async fn get_top100(
        &self,
        server: SekaiServerRegion,
        event_id: i64,
    ) -> Result<Top100RankingResponse, SekaiApiError> {
        let url = self.url(server, event_id, "ranking-top100");
        let bytes = self.fetch(&url).await?;
        sonic_rs::from_slice(&bytes).map_err(|source| SekaiApiError::Decode { url, source })
    }

    #[tracing::instrument(skip(self), fields(server = %server, event_id))]
    pub async fn get_border(
        &self,
        server: SekaiServerRegion,
        event_id: i64,
    ) -> Result<([u8; 32], BorderRankingResponse), SekaiApiError> {
        let url = self.url(server, event_id, "ranking-border");
        let bytes = self.fetch(&url).await?;
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let parsed =
            sonic_rs::from_slice(&bytes).map_err(|source| SekaiApiError::Decode { url, source })?;
        Ok((hash, parsed))
    }

    async fn fetch(&self, url: &str) -> Result<bytes::Bytes, SekaiApiError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|source| SekaiApiError::Request {
                url: url.to_string(),
                source,
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SekaiApiError::Status {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }
        resp.bytes().await.map_err(|source| SekaiApiError::Request {
            url: url.to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{StatusCode, Uri};
    use axum::response::Response;
    use axum::routing::get;

    async fn upstream(uri: Uri) -> Response {
        let path = uri.path();
        if path.contains("/event/500/") {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("upstream error"))
                .unwrap();
        }
        if path.contains("/event/501/") {
            return Response::new(Body::from("not-json"));
        }
        let body = if path.ends_with("ranking-border") {
            r#"{"eventId":7,"isEventAggregate":false,"borderRankings":[]}"#
        } else {
            r#"{"isEventAggregate":false,"rankings":[],"userRankingStatus":"normal"}"#
        };
        Response::new(Body::from(body))
    }

    async fn mock_upstream() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/{*path}", get(upstream)))
                .await
                .unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn fetches_decodes_hashes_and_classifies_failures() {
        let (base, task) = mock_upstream().await;
        let client = HarukiSekaiAPIClient::new(base, "").unwrap();

        let top = client.get_top100(SekaiServerRegion::Jp, 7).await.unwrap();
        assert!(top.rankings.is_empty());
        let (hash, border) = client.get_border(SekaiServerRegion::En, 7).await.unwrap();
        assert_eq!(border.event_id, Some(7));
        assert_ne!(hash, [0; 32]);
        assert!(matches!(
            client.get_top100(SekaiServerRegion::Jp, 500).await,
            Err(SekaiApiError::Status { status: 502, .. })
        ));
        assert!(matches!(
            client.get_border(SekaiServerRegion::Jp, 501).await,
            Err(SekaiApiError::Decode { .. })
        ));
        task.abort();

        let unavailable = HarukiSekaiAPIClient::new("http://127.0.0.1:1", "").unwrap();
        assert!(matches!(
            unavailable.get_top100(SekaiServerRegion::Jp, 1).await,
            Err(SekaiApiError::Request { .. })
        ));
    }
}
