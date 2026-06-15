use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

use crate::api::error::ApiError;
use crate::config::ApiQueryConfig;
use crate::model::enums::SekaiServerRegion;

#[derive(Clone)]
pub struct ApiQueryLimiter {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: ApiQueryConfig,
    global_trace: Option<Arc<Semaphore>>,
    per_server_trace: HashMap<SekaiServerRegion, Arc<Semaphore>>,
}

pub struct QueryPermit {
    _global: Option<OwnedSemaphorePermit>,
    _server: Option<OwnedSemaphorePermit>,
}

impl ApiQueryLimiter {
    pub fn new<I>(cfg: ApiQueryConfig, servers: I) -> Self
    where
        I: IntoIterator<Item = SekaiServerRegion>,
    {
        let global_trace = (cfg.trace_global_max_concurrency > 0)
            .then(|| Arc::new(Semaphore::new(cfg.trace_global_max_concurrency)));
        let per_server_trace = if cfg.trace_per_server_max_concurrency == 0 {
            HashMap::new()
        } else {
            servers
                .into_iter()
                .map(|server| {
                    (
                        server,
                        Arc::new(Semaphore::new(cfg.trace_per_server_max_concurrency)),
                    )
                })
                .collect()
        };
        Self {
            inner: Arc::new(Inner {
                cfg,
                global_trace,
                per_server_trace,
            }),
        }
    }

    pub async fn acquire_trace(&self, server: SekaiServerRegion) -> Result<QueryPermit, ApiError> {
        let global = acquire_optional(
            self.inner.global_trace.as_ref(),
            self.inner.cfg.acquire_timeout_ms,
        )
        .await?;
        let server = acquire_optional(
            self.inner.per_server_trace.get(&server),
            self.inner.cfg.acquire_timeout_ms,
        )
        .await?;
        Ok(QueryPermit {
            _global: global,
            _server: server,
        })
    }

    pub fn batch_trace_fill_concurrency(&self) -> usize {
        self.inner.cfg.batch_trace_fill_concurrency.max(1)
    }
}

async fn acquire_optional(
    semaphore: Option<&Arc<Semaphore>>,
    timeout_ms: u64,
) -> Result<Option<OwnedSemaphorePermit>, ApiError> {
    let Some(semaphore) = semaphore else {
        return Ok(None);
    };
    let acquire = semaphore.clone().acquire_owned();
    if timeout_ms == 0 {
        return acquire
            .await
            .map(Some)
            .map_err(|_| ApiError::ServiceUnavailable("trace query limiter closed".into()));
    }

    timeout(Duration::from_millis(timeout_ms), acquire)
        .await
        .map_err(|_| ApiError::ServiceUnavailable("trace query concurrency limit reached".into()))?
        .map(Some)
        .map_err(|_| ApiError::ServiceUnavailable("trace query limiter closed".into()))
}
