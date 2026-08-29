use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::future;
use serde::Serialize;
use std::time::Duration;
use tokio::time;

use crate::api::json::Json;
use crate::api::state::AppState;

const DB_PING_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Serialize)]
pub struct LiveResponse {
    status: &'static str,
}

#[derive(Serialize)]
pub struct ReadyResponse {
    status: &'static str,
    databases: Vec<DatabaseStatus>,
}

#[derive(Serialize)]
pub struct DatabaseStatus {
    server: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn livez() -> Json<LiveResponse> {
    Json(LiveResponse { status: "ok" })
}

pub async fn readyz(State(state): State<AppState>) -> Response {
    let checks = state.dbs().map(|(server, db)| async move {
        match time::timeout(DB_PING_TIMEOUT, db.ping()).await {
            Ok(Ok(())) => DatabaseStatus {
                server: server.to_string(),
                status: "ok",
                error: None,
            },
            Ok(Err(err)) => DatabaseStatus {
                server: server.to_string(),
                status: "error",
                error: Some(err.to_string()),
            },
            Err(_) => DatabaseStatus {
                server: server.to_string(),
                status: "error",
                error: Some(format!(
                    "database ping timed out after {}s",
                    DB_PING_TIMEOUT.as_secs()
                )),
            },
        }
    });
    let databases = future::join_all(checks).await;
    let ready = databases.iter().all(|db| db.status == "ok");

    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = Json(ReadyResponse {
        status: if ready { "ok" } else { "error" },
        databases,
    });
    (status, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::limiter::ApiQueryLimiter;
    use crate::api::private_lookup::PrivateLookupVerifier;
    use crate::api::realtime::RealtimeHub;
    use crate::api::ws_ticket::WsTicketStore;
    use crate::config::ApiQueryConfig;
    use crate::db::engine::DatabaseEngine;
    use crate::model::enums::SekaiServerRegion;
    use crate::privacy::UidAnonymizer;
    use sea_orm::{Database, DatabaseBackend};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn state_with_dbs(dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>>) -> AppState {
        AppState::new(
            dbs,
            None,
            ApiQueryLimiter::new(ApiQueryConfig::default(), [SekaiServerRegion::Jp]),
            UidAnonymizer::disabled(),
            Option::<PrivateLookupVerifier>::None,
            RealtimeHub::new(),
            WsTicketStore::default(),
        )
    }

    #[tokio::test]
    async fn liveness_and_empty_readiness_are_ok() {
        assert_eq!(livez().await.0.status, "ok");
        let response = readyz(State(state_with_dbs(HashMap::new()))).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readiness_reports_healthy_and_failed_databases() {
        let healthy = Arc::new(DatabaseEngine::from_connection(
            Database::connect("sqlite::memory:").await.unwrap(),
            DatabaseBackend::Sqlite,
        ));
        let response = readyz(State(state_with_dbs(HashMap::from([(
            SekaiServerRegion::Jp,
            healthy,
        )]))))
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let failed_conn = Database::connect("sqlite::memory:").await.unwrap();
        failed_conn.clone().close().await.unwrap();
        let failed = Arc::new(DatabaseEngine::from_connection(
            failed_conn,
            DatabaseBackend::Sqlite,
        ));
        let response = readyz(State(state_with_dbs(HashMap::from([(
            SekaiServerRegion::Jp,
            failed,
        )]))))
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
