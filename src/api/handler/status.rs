//! `/status` — heartbeat freshness for a (server, event). Mirrors
//! `getEventStatus`. `status==0` is healthy ("OK"), anything else is
//! reported as `"Error"` in the human-readable description.

use axum::extract::{Path, State};
use chrono::Utc;

use crate::api::error::ApiError;
use crate::api::extract::resolve_engine;
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::query::heartbeat::fetch_latest_heartbeat;
use crate::model::api::EventStatusResponseSchema;

#[tracing::instrument(skip(state), fields(server, event_id))]
pub async fn event_status(
    State(state): State<AppState>,
    Path((server, event_id)): Path<(String, i64)>,
) -> Result<Json<EventStatusResponseSchema>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let row = fetch_latest_heartbeat(&engine, event_id).await?;
    let Some((timestamp, status)) = row else {
        return Err(ApiError::NotFound);
    };
    let time_ago = Utc::now().timestamp() - timestamp;
    let status_desc = if status == 0 { "OK" } else { "Error" };
    Ok(Json(EventStatusResponseSchema {
        timestamp,
        status,
        status_desc: status_desc.to_owned(),
        time_ago,
    }))
}
