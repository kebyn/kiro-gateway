use crate::{
    AppState,
    error::AppError,
    protocol::{
        anthropic::{CountTokensRequest, MessagesRequest},
        internal::{InternalRequest, InternalResponse},
    },
    transform::converter::{anthropic_response, anthropic_stop_reason},
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
        let events = anthropic_stream_events(&payload, &response)
            .into_iter()
            .map(|(event, data)| Event::default().event(event).data(data.to_string()));
        Ok(Sse::new(stream::iter(events.into_iter().map(Ok::<Event, std::convert::Infallible>)))
            .into_response())
    } else {
        Ok(Json(payload).into_response())
    }
}

fn anthropic_stream_events(
    payload: &serde_json::Value,
    response: &InternalResponse,
) -> Vec<(&'static str, serde_json::Value)> {
    let mut events = vec![(
        "message_start",
        json!({
            "type":"message_start",
            "message": {
                "id": payload["id"],
                "type": "message",
                "role": "assistant",
                "model": payload["model"],
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": payload["usage"]["input_tokens"],
                    "output_tokens": 0,
                }
            }
        }),
    )];
    let mut index = 0;
    if !response.thinking.is_empty() {
        events.push((
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":""}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":response.thinking}}),
        ));
        events.push(("content_block_stop", json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    if !response.text.is_empty() {
        events.push((
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":response.text}}),
        ));
        events.push(("content_block_stop", json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    for call in &response.tool_calls {
        events.push((
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":call.id,"name":call.name,"input":{}}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":call.arguments_json()}}),
        ));
        events.push(("content_block_stop", json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    events.push((
        "message_delta",
        json!({
            "type":"message_delta",
            "delta":{"stop_reason":anthropic_stop_reason(response),"stop_sequence":null},
            "usage":{"output_tokens":payload["usage"]["output_tokens"]}
        }),
    ));
    events.push(("message_stop", json!({"type":"message_stop"})));
    events
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

#[cfg(test)]
mod tests {
    use super::anthropic_stream_events;
    use crate::{
        protocol::internal::{InternalResponse, InternalToolCall},
        transform::converter::anthropic_response,
    };
    use serde_json::json;

    #[test]
    fn streams_tool_use_blocks_with_protocol_indices() {
        let response = InternalResponse {
            tool_calls: vec![
                InternalToolCall {
                    id: "call_a".into(),
                    name: "alpha".into(),
                    arguments: json!({"a":1}),
                    complete: true,
                },
                InternalToolCall {
                    id: "call_b".into(),
                    name: "beta".into(),
                    arguments: json!({"b":2}),
                    complete: true,
                },
            ],
            ..Default::default()
        };
        let payload = anthropic_response("kiro", &response);
        let events = anthropic_stream_events(&payload, &response);
        assert_eq!(
            events.iter().map(|event| event.0).collect::<Vec<_>>(),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[2].1["delta"]["partial_json"], r#"{"a":1}"#);
        assert_eq!(events[4].1["index"], 1);
        assert_eq!(events[7].1["delta"]["stop_reason"], "tool_use");
    }
}
