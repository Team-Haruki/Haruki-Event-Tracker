//! `HarukiSekaiAPIClient` — single shared HTTP client for the Sekai
//! upstream. Direct port of `tracker/callapi.go::NewHarukiSekaiAPIClient`.
//!
//! `reqwest::Client` is `Clone` (cheap, internally `Arc`-shared), so all
//! per-server tracker daemons receive a clone of the same connection pool.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const TOKEN_HEADER: &str = "X-Haruki-Sekai-Token";

#[derive(Debug, Clone)]
pub struct HarukiSekaiAPIClient {
    pub(crate) http: reqwest::Client,
    pub(crate) api_endpoint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("invalid Sekai API token (must be ASCII, no CR/LF)")]
    InvalidToken,
    #[error("reqwest client build failed: {0}")]
    Build(#[from] reqwest::Error),
}

impl HarukiSekaiAPIClient {
    /// `api_endpoint` is the base URL with no trailing slash, e.g.
    /// `https://haruki-sekai.example.com`. `authorization` is empty for
    /// the public endpoint and goes into the `X-Haruki-Sekai-Token`
    /// header otherwise.
    pub fn new(api_endpoint: impl Into<String>, authorization: &str) -> Result<Self, BuildError> {
        let mut headers = HeaderMap::new();
        let ua = format!("Haruki-Event-Tracker/{}", env!("CARGO_PKG_VERSION"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&ua).map_err(|_| BuildError::InvalidToken)?,
        );
        if !authorization.is_empty() {
            let v = HeaderValue::from_str(authorization).map_err(|_| BuildError::InvalidToken)?;
            headers.insert(TOKEN_HEADER, v);
        }

        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .default_headers(headers)
            .build()?;

        let mut endpoint: String = api_endpoint.into();
        while endpoint.ends_with('/') {
            endpoint.pop();
        }

        Ok(Self {
            http,
            api_endpoint: endpoint,
        })
    }
}
