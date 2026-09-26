use crate::{AppState, error::AppError};
use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};
use serde_json::Value;

#[path = "responses/handler.rs"]
mod handler;

#[cfg(test)]
use handler::create;
pub use handler::create_route;

#[path = "responses/events.rs"]
mod events;
#[path = "responses/lifecycle.rs"]
mod lifecycle;
#[path = "responses/live.rs"]
mod live;
#[path = "responses/payload.rs"]
mod payload;
#[path = "responses/state.rs"]
mod state;

#[cfg(test)]
use events::responses_stream_events;
#[cfg(test)]
use live::responses_live_stream;
#[cfg(test)]
use payload::{response_messages, responses_payload, responses_payload_with_tools};

pub async fn get(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let record = state.responses.get(&id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(record.payload.get("response").cloned().unwrap_or(Value::Null)))
}
pub async fn delete(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if state.responses.delete(&id).await? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[cfg(test)]
#[path = "responses/tests.rs"]
mod tests;
