use crate::{
    AppState,
    error::AppError,
    protocol::{
        anthropic::{CountTokensRequest, MessagesRequest},
        internal::InternalRequest,
    },
    transform::converter::anthropic_response,
};
use axum::{
    Json,
    extract::State,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::stream;
use serde_json::json;

pub async fn messages(
    State(state): State<AppState>,
    Json(body): Json<MessagesRequest>,
) -> Result<Response, AppError> {
    let request: InternalRequest = body.into();
    let stream_response = request.stream;
    let model = request.model.clone();
    let response = state.complete(&request).await?;
    let payload = anthropic_response(&model, &response);
    if stream_response {
        let mut events = vec![
            Event::default().event("message_start").data(
                serde_json::to_string(&json!({"type":"message_start","message":payload}))
                    .map_err(|e| AppError::Internal(e.to_string()))?,
            ),
        ];
        if !response.text.is_empty() {
            events.push(Event::default().event("content_block_start").data(
                serde_json::to_string(&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}))
                    .map_err(|e| AppError::Internal(e.to_string()))?,
            ));
            events.push(Event::default().event("content_block_delta").data(
                serde_json::to_string(&json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":response.text}}))
                    .map_err(|e| AppError::Internal(e.to_string()))?,
            ));
            events.push(
                Event::default().event("content_block_stop").data(
                    serde_json::to_string(&json!({"type":"content_block_stop","index":0}))
                        .map_err(|e| AppError::Internal(e.to_string()))?,
                ),
            );
        }
        events.push(Event::default().event("message_delta").data(
            serde_json::to_string(&json!({"type":"message_delta","delta":{"stop_reason":payload["stop_reason"],"stop_sequence":null},"usage":payload["usage"]}))
                .map_err(|e| AppError::Internal(e.to_string()))?,
        ));
        events.push(
            Event::default().event("message_stop").data(
                serde_json::to_string(&json!({"type":"message_stop"}))
                    .map_err(|e| AppError::Internal(e.to_string()))?,
            ),
        );
        Ok(Sse::new(stream::iter(events.into_iter().map(Ok::<Event, std::convert::Infallible>)))
            .into_response())
    } else {
        Ok(Json(payload).into_response())
    }
}
pub async fn count_tokens(
    Json(body): Json<CountTokensRequest>,
) -> Result<impl IntoResponse, AppError> {
    let text = body
        .messages
        .iter()
        .map(|message| match &message.content {
            serde_json::Value::String(v) => v.clone(),
            v => v.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let tools = serde_json::to_string(&body.tools).unwrap_or_default();
    Ok(Json(json!({"input_tokens": (text.chars().count() + tools.chars().count()) as u64 / 4 + 1})))
}
