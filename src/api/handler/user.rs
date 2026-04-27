//! `user-data/:user_id` — direct lookup against the per-event users
//! table. Mirrors `getUserDataByUserID`.

use axum::extract::{Path, State};

use crate::api::error::ApiError;
use crate::api::extract::resolve_engine;
use crate::api::json::Json;
use crate::api::state::AppState;
use crate::db::query::user::get_user_data;
use crate::model::api::RecordedUserNameSchema;

#[tracing::instrument(skip(state), fields(server, event_id, user_id))]
pub async fn user_data(
    State(state): State<AppState>,
    Path((server, event_id, user_id)): Path<(String, i64, String)>,
) -> Result<Json<RecordedUserNameSchema>, ApiError> {
    let engine = resolve_engine(&state, &server)?;
    let user = get_user_data(&engine, event_id, &user_id).await?;
    user.map(Json).ok_or(ApiError::NotFound)
}
