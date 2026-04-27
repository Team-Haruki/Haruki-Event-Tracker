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
        format!("{}/{}/event/{}/{}", self.api_endpoint, server, event_id, leaf)
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
        let parsed = sonic_rs::from_slice(&bytes)
            .map_err(|source| SekaiApiError::Decode { url, source })?;
        Ok((hash, parsed))
    }

    async fn fetch(&self, url: &str) -> Result<bytes::Bytes, SekaiApiError> {
        let resp = self.http.get(url).send().await.map_err(|source| {
            SekaiApiError::Request {
                url: url.to_string(),
                source,
            }
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
