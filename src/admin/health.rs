use crate::{AppState, error::AppError};
use axum::{Json, extract::State, response::IntoResponse};
use chrono::Utc;

pub async fn provider_health(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        serde_json::json!({"ok": state.token_manager.status().has_access_token || state.token_manager.status().has_refresh_token, "credential": state.token_manager.status(), "refresh": state.token_manager.refresh_state(), "time": Utc::now()}),
    ))
}
