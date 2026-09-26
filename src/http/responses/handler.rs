use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::{
    AppState,
    error::{AppError, Protocol, protocol_error_response},
    generation::GenerationResult,
    protocol::openai_responses::ResponsesRequest,
    response_store::{ResponseStatus, ResponseStore},
};

use super::{
    events::responses_stream_events,
    live::responses_live_stream,
    payload::{
        response_failed_payload, response_in_progress_payload, response_messages,
        responses_payload_with_tools,
    },
};

pub async fn create_route(
    State(state): State<AppState>,
    body: Result<Json<ResponsesRequest>, JsonRejection>,
) -> Response {
    match body {
        Ok(body) => match create(State(state), body).await {
            Ok(response) => response,
            Err(error) => protocol_error_response(Protocol::Responses, error),
        },
        Err(rejection) => {
            let message = rejection.to_string();
            let error = if message.to_ascii_lowercase().contains("limit")
                || message.to_ascii_lowercase().contains("too large")
            {
                AppError::PayloadTooLarge
            } else {
                AppError::BadRequest(format!("invalid JSON request: {message}"))
            };
            protocol_error_response(Protocol::Responses, error)
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<ResponsesRequest>,
) -> Result<Response, AppError> {
    body.validate().map_err(AppError::BadRequest)?;
    let previous_record = match body.previous_response_id.as_deref() {
        Some(id) => Some(state.responses.get(id)?.ok_or(AppError::NotFound)?),
        None => None,
    };
    let previous =
        previous_record.as_ref().map(ResponseStore::extract_messages).unwrap_or_default();
    let previous_tools =
        previous_record.as_ref().map(ResponseStore::extract_tools).unwrap_or_default();
    let store = body.store;
    let stream_response = body.stream;
    let mut internal = body.into_generation(previous).map_err(AppError::BadRequest)?;
    if internal.tools.is_empty() {
        internal.tools = previous_tools;
    }
    internal.model = state.token_manager.resolve_model(&internal.model).await?;
    tracing::debug!(
        protocol = "openai_responses",
        model = %internal.model,
        stream = internal.stream,
        store,
        tools = internal.tools.len(),
        "accepted OpenAI Responses request"
    );
    let model = internal.model.clone();
    let id = format!("resp_{}", uuid::Uuid::now_v7());
    let stored_messages = response_messages(&internal.messages, &GenerationResult::default());
    let initial_payload = response_in_progress_payload(&id, &model);
    let mut record = None;
    if store {
        let created = state.responses.create_with_id(
            id.clone(),
            &model,
            json!({"messages":stored_messages,"tools":internal.tools,"response":initial_payload}),
            ResponseStatus::InProgress,
        )?;
        record = Some(created);
        if !stream_response {
            state.responses.append_event(
                &id,
                "response.created",
                &json!({"response":response_in_progress_payload(&id, &model)}),
            )?;
            state.responses.append_event(
                &id,
                "response.in_progress",
                &json!({"response":response_in_progress_payload(&id, &model)}),
            )?;
        }
    }
    if stream_response {
        let upstream = match state.event_stream(&internal).await {
            Ok(upstream) => upstream,
            Err(error) => {
                if let Some(record) = record {
                    let _ = state.responses.append_event(
                        &id,
                        "response.created",
                        &json!({"response":response_in_progress_payload(&id, &model)}),
                    );
                    let _ = state.responses.append_event(
                        &id,
                        "response.in_progress",
                        &json!({"response":response_in_progress_payload(&id, &model)}),
                    );
                    let failed = response_failed_payload(&id, &model, &error.to_string());
                    let _ = state.responses.update(
                        record,
                        ResponseStatus::Failed,
                        json!({"messages":stored_messages,"tools":internal.tools,"response":failed}),
                    );
                    let _ = state.responses.append_event(&id, "response.failed", &failed);
                }
                return Err(error);
            }
        };
        return Ok(responses_live_stream(state, upstream, id, model, internal, store, record)
            .into_response());
    }
    let response = match state.complete(&internal).await {
        Ok(response) => response,
        Err(error) => {
            if let Some(record) = record {
                let failed = response_failed_payload(&id, &model, &error.to_string());
                let _ = state.responses.update(
                    record,
                    ResponseStatus::Failed,
                    json!({"messages":stored_messages,"tools":internal.tools,"response":failed}),
                );
                let _ = state.responses.append_event(&id, "response.failed", &failed);
            }
            return Err(error);
        }
    };
    let payload = responses_payload_with_tools(&id, &model, &internal.tools, &response);
    if let Some(record) = record {
        let status = if response.incomplete {
            ResponseStatus::Incomplete
        } else {
            ResponseStatus::Completed
        };
        let messages = response_messages(&internal.messages, &response);
        for (event_type, event_payload) in responses_stream_events(&payload) {
            state.responses.append_event(&id, event_type, &event_payload)?;
        }
        let _ = state.responses.update(
            record,
            status,
            json!({"messages":messages,"tools":internal.tools,"response":payload}),
        )?;
    }
    Ok(Json(payload).into_response())
}
