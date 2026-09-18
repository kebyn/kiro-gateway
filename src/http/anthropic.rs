use crate::transform::truncation::XmlLeakFilter;
use crate::{
    AppState,
    error::AppError,
    protocol::{
        anthropic::{CountTokensRequest, MessagesRequest},
        internal::{InternalEvent, InternalRequest, InternalResponse},
    },
    transform::converter::{anthropic_response, anthropic_stop_reason},
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
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
};

pub async fn messages(
    State(state): State<AppState>,
    Json(body): Json<MessagesRequest>,
) -> Result<Response, AppError> {
    let request: InternalRequest = body.into();
    let stream_response = request.stream;
    let model = request.model.clone();
    if !stream_response {
        let response = state.complete(&request).await?;
        return Ok(Json(anthropic_response(&model, &response)).into_response());
    }
    let upstream = state.event_stream(&request).await?;
    let message_id = format!("msg_{}", uuid::Uuid::now_v7());
    let input_tokens = request.input_text().chars().count() as u64 / 4;
    let stream = async_stream::stream! {
        let start = json!({
            "type":"message_start",
            "message": {
                "id":message_id,
                "type":"message",
                "role":"assistant",
                "model":model,
                "content":[],
                "stop_reason":null,
                "stop_sequence":null,
                "usage":{"input_tokens":input_tokens,"output_tokens":0}
            }
        });
        yield Ok::<Event, Infallible>(Event::default().event("message_start").data(start.to_string()));

        let mut upstream = upstream;
        let mut accumulator = InternalEventAccumulator::new();
        let mut text_index = None;
        let mut thinking_index = None;
        let mut tool_indices = HashMap::<String, usize>::new();
        let mut active_tool = None::<String>;
        let mut block_order = Vec::<usize>::new();
        let mut closed_blocks = HashSet::<usize>::new();
        let mut next_index = 0_usize;
        let mut text_filter = XmlLeakFilter::new();
        let mut failed = false;

        while let Some(item) = upstream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(error) => {
                    yield Ok(Event::default().event("error").data(json!({"type":"error","error":{"type":"upstream_error","message":error.to_string()}}).to_string()));
                    yield Ok(Event::default().event("message_delta").data(json!({"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":0}}).to_string()));
                    yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
                    failed = true;
                    break;
                }
            };
            if let InternalEvent::Error { message } = &event {
                yield Ok(Event::default().event("error").data(json!({"type":"error","error":{"type":"upstream_error","message":message}}).to_string()));
                yield Ok(Event::default().event("message_delta").data(json!({"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":0}}).to_string()));
                yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
                failed = true;
                break;
            }
            if let Err(error) = accumulator.push(event.clone()) {
                yield Ok(Event::default().event("error").data(json!({"type":"error","error":{"type":"upstream_error","message":error.to_string()}}).to_string()));
                yield Ok(Event::default().event("message_delta").data(json!({"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":0}}).to_string()));
                yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
                failed = true;
                break;
            }
            match event {
                InternalEvent::TextDelta { text } => {
                    let was_none = text_index.is_none();
                    let index = *text_index.get_or_insert_with(|| {
                        let value = next_index;
                        next_index += 1;
                        value
                    });
                    if was_none {
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}).to_string()));
                    }
                    let text = text_filter.push(&text);
                    if !text.is_empty() {
                        yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}).to_string()));
                    }
                }
                InternalEvent::ThinkingDelta { text } => {
                    let was_none = thinking_index.is_none();
                    let index = *thinking_index.get_or_insert_with(|| {
                        let value = next_index;
                        next_index += 1;
                        value
                    });
                    if was_none {
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":""}}).to_string()));
                    }
                    if !text.is_empty() {
                        yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":text}}).to_string()));
                    }
                }
                InternalEvent::ToolCallStart { id, name } => {
                    let key = if id.is_empty() {
                        active_tool.clone().unwrap_or_else(|| format!("tool_call_{}", tool_indices.len() + 1))
                    } else { id };
                    if !tool_indices.contains_key(&key) {
                        let index = next_index;
                        next_index += 1;
                        tool_indices.insert(key.clone(), index);
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":key,"name":name,"input":{}}}).to_string()));
                    }
                    active_tool = Some(key);
                }
                InternalEvent::ToolCallDelta { id, arguments, name } => {
                    let mut key = id;
                    if key.is_empty() {
                        key = active_tool.clone().unwrap_or_else(|| format!("tool_call_{}", tool_indices.len() + 1));
                    }
                    if !tool_indices.contains_key(&key) {
                        let index = next_index;
                        next_index += 1;
                        tool_indices.insert(key.clone(), index);
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":key,"name":name.unwrap_or_default(),"input":{}}}).to_string()));
                    }
                    active_tool = Some(key.clone());
                    if !arguments.is_empty() {
                        yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":tool_indices[&key],"delta":{"type":"input_json_delta","partial_json":arguments}}).to_string()));
                    }
                }
                InternalEvent::ToolCallEnd { id, .. } => {
                    let key = if id.is_empty() { active_tool.clone() } else { Some(id) };
                    if let Some(key) = key {
                        if let Some(index) = tool_indices.get(&key).copied() {
                            if closed_blocks.insert(index) {
                                yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
                            }
                        }
                        if active_tool.as_deref() == Some(key.as_str()) { active_tool = None; }
                    }
                }
                InternalEvent::Usage { .. } | InternalEvent::Stop { .. } => {}
                InternalEvent::Error { .. } => unreachable!(),
            }
        }
        if failed { return; }
        let response = accumulator.finish();
        for index in block_order {
            if !closed_blocks.contains(&index) {
                yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
            }
        }
        let output_tokens = response.usage.as_ref().map_or(0, |usage| usage.output_tokens);
        yield Ok(Event::default().event("message_delta").data(json!({"type":"message_delta","delta":{"stop_reason":anthropic_stop_reason(&response),"stop_sequence":null},"usage":{"output_tokens":output_tokens}}).to_string()));
        yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
    };
    Ok(Sse::new(stream).into_response())
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
