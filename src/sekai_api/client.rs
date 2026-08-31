//! `HarukiSekaiAPIClient` — single shared HTTP client for the Sekai
//! upstream. Direct port of `tracker/callapi.go::NewHarukiSekaiAPIClient`.
//!
//! `reqwest::Client` is `Clone` (cheap, internally `Arc`-shared), so all
//! per-server tracker daemons receive a clone of the same connection pool.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};

// The tracker can run at second-level cadence; a hung upstream must fail
// fast enough to land an error heartbeat and recover the tick rhythm
// instead of pinning the daemon mutex for tens of skipped ticks.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
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
        Self::with_timeouts(
            api_endpoint,
            authorization,
            DEFAULT_TIMEOUT,
            DEFAULT_CONNECT_TIMEOUT,
        )
    }

    /// `new` with explicit timeouts (a zero duration falls back to the
    /// default). Wired from the `sekai_api` config section.
    pub fn with_timeouts(
        api_endpoint: impl Into<String>,
        authorization: &str,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self, BuildError> {
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

        let timeout = if timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            timeout
        };
        let connect_timeout = if connect_timeout.is_zero() {
            DEFAULT_CONNECT_TIMEOUT
        } else {
            connect_timeout
        };
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
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
