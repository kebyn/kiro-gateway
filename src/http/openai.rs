use crate::{
    AppState,
    error::AppError,
    protocol::{
        internal::{InternalEvent, InternalRequest, InternalResponse},
        openai_chat::ChatRequest,
    },
    transform::converter::{chat_finish_reason, openai_chat_response},
    upstream::request::InternalEventAccumulator,
};
use axum::{
    Json,
    extract::State,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::StreamExt;
use serde_json::json;
use std::{collections::HashMap, convert::Infallible};

pub async fn chat_completions(
    State(state): State<AppState>,
    Json(body): Json<ChatRequest>,
) -> Result<Response, AppError> {
    let request: InternalRequest = body.into();
    let stream_response = request.stream;
    let model = request.model.clone();
    if !stream_response {
        let response = state.complete(&request).await?;
        return Ok(Json(openai_chat_response(&model, &response)).into_response());
    }
    let upstream = state.event_stream(&request).await?;
    let id = format!("chatcmpl-{}", uuid::Uuid::now_v7());
    let created = chrono::Utc::now().timestamp();
    let stream = async_stream::stream! {
        let mut upstream = upstream;
        let first = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]});
        yield Ok::<Event, Infallible>(Event::default().data(first.to_string()));
        let mut accumulator = InternalEventAccumulator::new();
        let mut tool_indices = HashMap::<String, usize>::new();
        let mut next_tool_index = 0_usize;
        let mut failed = false;
        while let Some(item) = upstream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(error) => {
                    yield Ok(Event::default().event("error").data(json!({"error":{"message":error.to_string(),"type":"upstream_error"}}).to_string()));
                    yield Ok(Event::default().data("[DONE]"));
                    failed = true;
                    break;
                }
            };
            if let InternalEvent::Error { message } = &event {
                yield Ok(Event::default().event("error").data(json!({"error":{"message":message,"type":"upstream_error"}}).to_string()));
                yield Ok(Event::default().data("[DONE]"));
                failed = true;
                break;
            }
            if let Err(error) = accumulator.push(event.clone()) {
                yield Ok(Event::default().event("error").data(json!({"error":{"message":error.to_string(),"type":"upstream_error"}}).to_string()));
                yield Ok(Event::default().data("[DONE]"));
                failed = true;
                break;
            }
            let chunk = |delta: serde_json::Value| {
                json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
            };
            match event {
                InternalEvent::TextDelta { text } if !text.is_empty() => {
                    yield Ok(Event::default().data(chunk(json!({"content":text})).to_string()));
                }
                InternalEvent::ThinkingDelta { text } if !text.is_empty() => {
                    yield Ok(Event::default().data(chunk(json!({"reasoning_content":text})).to_string()));
                }
                InternalEvent::ToolCallStart { id: call_id, name } => {
                    let index = *tool_indices.entry(call_id.clone()).or_insert_with(|| {
                        let value = next_tool_index;
                        next_tool_index += 1;
                        value
                    });
                    yield Ok(Event::default().data(chunk(json!({"tool_calls":[{"index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":""}}]})).to_string()));
                }
                InternalEvent::ToolCallDelta { id: call_id, arguments, name } => {
                    let index = *tool_indices.entry(call_id.clone()).or_insert_with(|| {
                        let value = next_tool_index;
                        next_tool_index += 1;
                        value
                    });
                    let mut function = json!({"arguments":arguments});
                    if let Some(name) = name { function["name"] = json!(name); }
                    yield Ok(Event::default().data(chunk(json!({"tool_calls":[{"index":index,"function":function}]})).to_string()));
                }
                InternalEvent::ToolCallEnd { .. } | InternalEvent::Usage { .. } | InternalEvent::Stop { .. } => {}
                InternalEvent::Error { .. } | InternalEvent::TextDelta { .. } | InternalEvent::ThinkingDelta { .. } => {}
            }
        }
        if failed { return; }
        let response = accumulator.finish();
        let finish = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":chat_finish_reason(&response)}]});
        yield Ok(Event::default().data(finish.to_string()));
        yield Ok(Event::default().data("[DONE]"));
    };
    Ok(Sse::new(stream).into_response())
}

fn chat_stream_data(payload: &serde_json::Value, response: &InternalResponse) -> Vec<String> {
    let chunk = |delta: serde_json::Value, finish_reason: serde_json::Value| {
        json!({
            "id":payload["id"],
            "object":"chat.completion.chunk",
            "created":payload["created"],
            "model":payload["model"],
            "choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}]
        })
        .to_string()
    };
    let mut role_delta = json!({"role":"assistant"});
    if !response.text.is_empty() {
        role_delta["content"] = serde_json::Value::String(response.text.clone());
    }
    let mut chunks = vec![chunk(role_delta, serde_json::Value::Null)];
    for (index, call) in response.tool_calls.iter().enumerate() {
        chunks.push(chunk(
            json!({
                "tool_calls":[{
                    "index":index,
                    "id":call.id,
                    "type":"function",
                    "function":{"name":call.name,"arguments":call.arguments_json()}
                }]
            }),
            serde_json::Value::Null,
        ));
    }
    chunks.push(chunk(json!({}), json!(chat_finish_reason(response))));
    chunks.push("[DONE]".into());
    chunks
}

#[cfg(test)]
mod tests {
    use super::chat_stream_data;
    use crate::{
        protocol::internal::{InternalResponse, InternalToolCall},
        transform::converter::openai_chat_response,
    };
    use serde_json::{Value, json};

    #[test]
    fn streams_parallel_tool_calls_before_done() {
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
        let payload = openai_chat_response("kiro", &response);
        let data = chat_stream_data(&payload, &response);
        let chunks: Vec<Value> =
            data[..data.len() - 1].iter().map(|item| serde_json::from_str(item).unwrap()).collect();
        assert!(chunks[0]["choices"][0]["delta"].get("content").is_none());
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            r#"{"a":1}"#
        );
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
        assert_eq!(chunks[3]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(data.last().unwrap(), "[DONE]");
    }
}
