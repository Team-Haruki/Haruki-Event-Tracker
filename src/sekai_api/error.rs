//! Sekai API client error type. Mirrors the failure modes of the Go
//! client (`tracker/callapi.go`): transport failures, non-200 statuses,
//! and JSON decode failures, distinguished so the tracker can log them
//! differently and still emit a heartbeat on either category.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SekaiApiError {
    #[error("sekai api request failed for {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("sekai api returned status {status} for {url}")]
    Status { status: u16, url: String },
    #[error("sekai api response decode failed for {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: sonic_rs::Error,
    },
}
