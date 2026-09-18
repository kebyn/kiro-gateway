use crate::{
    AppState,
    error::AppError,
    protocol::{internal::InternalMessage, openai_responses::ResponsesRequest},
    response_store::{ResponseStatus, ResponseStore},
    transform::converter::openai_chat_response,
};
use axum::{
    Json,
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::stream;
use serde_json::{Value, json};

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<ResponsesRequest>,
) -> Result<Response, AppError> {
    let previous = body
        .previous_response_id
        .as_deref()
        .and_then(|id| state.responses.get(id).ok().flatten())
        .map(|record| ResponseStore::extract_messages(&record))
        .unwrap_or_default();
    let store = body.store;
    let stream_response = body.stream;
    let internal = body.into_internal(previous);
    let model = internal.model.clone();
    let response = state.complete(&internal).await?;
    let id = format!("resp_{}", uuid::Uuid::now_v7());
    let payload = json!({"id":id,"object":"response","status":if response.incomplete {"incomplete"} else {"completed"},"model":model,"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":response.text}]}],"output_text":response.text,"usage":response.usage,"tool_calls":response.tool_calls});
    if store {
        let mut record = state.responses.create_with_id(
            id.clone(),
            &model,
            json!({"messages":internal.messages,"response":payload}),
            ResponseStatus::InProgress,
        )?;
        let status = if response.incomplete {
            ResponseStatus::Incomplete
        } else {
            ResponseStatus::Completed
        };
        record = state.responses.update(
            record,
            status,
            json!({"messages":internal.messages,"response":payload}),
        )?;
        let _ = record;
    }
    if stream_response {
        let events = vec![
            Event::default().event("response.created").data(
                serde_json::to_string(&json!({"type":"response.created","response":payload}))
                    .map_err(|e| AppError::Internal(e.to_string()))?,
            ),
            Event::default().event("response.output_text.delta").data(
                serde_json::to_string(
                    &json!({"type":"response.output_text.delta","delta":response.text}),
                )
                .map_err(|e| AppError::Internal(e.to_string()))?,
            ),
            Event::default()
                .event(if response.incomplete {
                    "response.incomplete"
                } else {
                    "response.completed"
                })
                .data(
                    serde_json::to_string(&payload.clone())
                        .map_err(|e| AppError::Internal(e.to_string()))?,
                ),
        ];
        Ok(Sse::new(stream::iter(events.into_iter().map(Ok::<Event, std::convert::Infallible>)))
            .into_response())
    } else {
        Ok(Json(payload).into_response())
    }
}
pub async fn get(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let record = state.responses.get(&id)?.ok_or(AppError::NotFound)?;
    Ok(Json(record.payload.get("response").cloned().unwrap_or(Value::Null)))
}
pub async fn delete(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if state.responses.delete(&id)? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}
