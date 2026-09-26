use std::{convert::Infallible, sync::Arc};

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{
    AppState,
    generation::{GenerationEvent, GenerationResult, Message, ToolCall, ToolDefinition},
    response_store::ResponseStatus,
    transform::{converter::responses_incomplete_reason, truncation::XmlLeakFilter},
    upstream::GenerationAccumulator,
};

use super::{
    events::attach_sequence,
    lifecycle::{IncompleteRecordGuard, ResponseSnapshot},
    payload::{
        response_error_payload, response_failed_payload, response_in_progress_payload,
        response_in_progress_payload_at, response_messages,
    },
    state::{CustomToolInfo, LiveItem, LiveReasoning, LiveResponseState},
};

pub(super) fn custom_tool_info(name: &str, tools: &[ToolDefinition]) -> Option<CustomToolInfo> {
    tools.iter().find(|tool| tool.custom && tool.name == name).map(|tool| CustomToolInfo {
        name: tool.original_name.clone().unwrap_or_else(|| tool.name.clone()),
        namespace: tool.namespace.clone(),
    })
}

fn custom_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("input").and_then(Value::as_str).map(ToOwned::to_owned))
        .unwrap_or_else(|| arguments.to_owned())
}

fn custom_item(
    id: &str,
    call_id: &str,
    name: &str,
    namespace: Option<&str>,
    input: &str,
    status: &str,
) -> Value {
    let mut item = json!({
        "type":"custom_tool_call",
        "id":id,
        "call_id":call_id,
        "name":name,
        "input":input,
        "status":status,
    });
    if let Some(namespace) = namespace {
        item["namespace"] = json!(namespace);
    }
    item
}

pub(super) fn reasoning_item(reasoning: &LiveReasoning, status: &str) -> Value {
    json!({
        "type":"reasoning",
        "id":reasoning.item_id,
        "status":status,
        "summary":[{
            "type":"summary_text",
            "text":reasoning.summary
        }]
    })
}

pub(super) struct ToolItemSpec<'a> {
    pub(super) id: &'a str,
    pub(super) call_id: &'a str,
    pub(super) name: &'a str,
    pub(super) arguments: &'a str,
    pub(super) status: &'a str,
    pub(super) custom: bool,
    pub(super) response_name: &'a str,
    pub(super) namespace: Option<&'a str>,
}

pub(super) fn tool_item(spec: ToolItemSpec<'_>) -> Value {
    if spec.custom {
        custom_item(
            spec.id,
            spec.call_id,
            spec.response_name,
            spec.namespace,
            &custom_input(spec.arguments),
            spec.status,
        )
    } else {
        json!({
            "type":"function_call",
            "id":spec.id,
            "call_id":spec.call_id,
            "name":spec.name,
            "arguments":spec.arguments,
            "status":spec.status
        })
    }
}

pub(super) fn responses_live_stream(
    state: AppState,
    mut upstream: crate::upstream::GenerationEventStream,
    id: String,
    model: String,
    internal: crate::generation::GenerationRequest,
    store: bool,
    record: Option<crate::response_store::ResponseRecord>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let stored_messages = response_messages(&internal.messages, &GenerationResult::default());
    let stored_tools = internal.tools.clone();
    let snapshot = Arc::new(Mutex::new(ResponseSnapshot {
        messages: stored_messages.clone(),
        tools: stored_tools.clone(),
        response: response_in_progress_payload(&id, &model),
        status: ResponseStatus::InProgress,
    }));
    let disconnect_guard = record.as_ref().map(|_| IncompleteRecordGuard {
        store: store.then_some(state.responses.clone()),
        record_id: id.clone(),
        snapshot: snapshot.clone(),
    });
    let created_at = record
        .as_ref()
        .and_then(|record| record.payload["response"]["created_at"].as_i64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let stream = async_stream::stream! {
        let _disconnect_guard = disconnect_guard;
        let mut sequence = 0_u64;
        let mut live = LiveResponseState::default();
        let mut accumulator = GenerationAccumulator::new();
        let mut text_filter = XmlLeakFilter::new();
        let mut failed = false;
        let initial = response_in_progress_payload_at(&id, &model, created_at);
        for (event_type, data) in [
            ("response.created", json!({"response":initial.clone()})),
            ("response.in_progress", json!({"response":initial})),
        ] {
            match attach_sequence(&state, &id, store, &mut sequence, event_type, data) {
                Ok(data) => yield Ok::<Event, Infallible>(Event::default().event(event_type).data(data.to_string())),
                Err(error) => {
                    yield Ok(Event::default().event("response.incomplete").data(response_error_payload(&id, &model, &error.to_string()).to_string()));
                    failed = true;
                    break;
                }
            }
        }
        if failed { return; }

        while let Some(item) = upstream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(error) => {
                    let payload = response_failed_payload(&id, &model, &error.to_string());
                    if store {
                        if let Some(record) = record.clone() {
                            let _ = state.responses.update(record, ResponseStatus::Failed, json!({"messages":stored_messages,"tools":stored_tools,"response":payload}));
                        }
                    }
                    if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.failed", json!({"response":payload,"error":{"code":"upstream_error","message":error.to_string()}})) {
                        yield Ok(Event::default().event("response.failed").data(data.to_string()));
                    }
                    failed = true;
                    break;
                }
            };
            if let GenerationEvent::Error { message } = &event {
                let payload = response_failed_payload(&id, &model, message);
                if store {
                    if let Some(record) = record.clone() {
                        let _ = state.responses.update(record, ResponseStatus::Failed, json!({"messages":stored_messages,"tools":stored_tools,"response":payload}));
                    }
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.failed", json!({"response":payload,"error":{"code":"upstream_error","message":message}})) {
                    yield Ok(Event::default().event("response.failed").data(data.to_string()));
                }
                failed = true;
                break;
            }
            if let Err(error) = accumulator.push(event.clone()) {
                let payload = response_failed_payload(&id, &model, &error.to_string());
                if store {
                    if let Some(record) = record.clone() {
                        let _ = state.responses.update(record, ResponseStatus::Failed, json!({"messages":stored_messages,"tools":stored_tools,"response":payload}));
                    }
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.failed", json!({"response":payload,"error":{"code":"upstream_error","message":error.to_string()}})) {
                    yield Ok(Event::default().event("response.failed").data(data.to_string()));
                }
                failed = true;
                break;
            }
            match event {
                GenerationEvent::TextDelta { text } => {
                    let text = text_filter.push(&text);
                    live.text.push_str(&text);
                    let (item_id, output_index, added) = live.ensure_text();
                    if added {
                        let item = json!({"type":"message","id":item_id,"status":"in_progress","role":"assistant","content":[]});
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                        }
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.content_part.added", json!({"item_id":item_id,"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}})) {
                            yield Ok(Event::default().event("response.content_part.added").data(data.to_string()));
                        }
                    }
                    if !text.is_empty() {
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_text.delta", json!({"item_id":item_id,"output_index":output_index,"content_index":0,"delta":text,"logprobs":[]})) {
                            yield Ok(Event::default().event("response.output_text.delta").data(data.to_string()));
                        }
                    }
                }
                GenerationEvent::ToolCallStart { id: call_id, name } => {
                    let key = if call_id.is_empty() { format!("tool_call_{}", live.tools.len() + 1) } else { call_id };
                    let custom = custom_tool_info(&name, &stored_tools);
                    let (tool_index, added) = live.ensure_tool(&key, &name, custom);
                    if added {
                        let tool = &live.tools[tool_index];
                        let item = tool_item(ToolItemSpec {
                            id: &tool.item_id,
                            call_id: &tool.call_id,
                            name: &tool.name,
                            arguments: "",
                            status: "in_progress",
                            custom: tool.custom,
                            response_name: &tool.response_name,
                            namespace: tool.namespace.as_deref(),
                        });
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":tool.output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                        }
                    }
                }
                GenerationEvent::ToolCallDelta { id: call_id, arguments, name } => {
                    let key = if call_id.is_empty() { live.tools.last().map(|tool| tool.call_id.clone()).unwrap_or_else(|| format!("tool_call_{}", live.tools.len() + 1)) } else { call_id };
                    let tool_name = name.as_deref().unwrap_or_default();
                    let custom = custom_tool_info(tool_name, &stored_tools);
                    let (tool_index, added) = live.ensure_tool(&key, tool_name, custom);
                    if added {
                        let tool = &live.tools[tool_index];
                        let item = tool_item(ToolItemSpec {
                            id: &tool.item_id,
                            call_id: &tool.call_id,
                            name: &tool.name,
                            arguments: "",
                            status: "in_progress",
                            custom: tool.custom,
                            response_name: &tool.response_name,
                            namespace: tool.namespace.as_deref(),
                        });
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":tool.output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                        }
                    }
                    if !arguments.is_empty() {
                        live.tools[tool_index].arguments.push_str(&arguments);
                    }
                    if !arguments.is_empty() && !live.tools[tool_index].custom {
                        let tool = &live.tools[tool_index];
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.function_call_arguments.delta", json!({"item_id":tool.item_id,"call_id":tool.call_id,"output_index":tool.output_index,"delta":arguments})) {
                            yield Ok(Event::default().event("response.function_call_arguments.delta").data(data.to_string()));
                        }
                    }
                }
                GenerationEvent::ToolCallEnd { id: call_id, complete } => {
                    let key = if call_id.is_empty() { live.tools.last().map(|tool| tool.call_id.clone()) } else { Some(call_id) };
                    if let Some(key) = key {
                        if let Some(tool_index) = live.tool_indices.get(&key).copied() {
                            let tool = &mut live.tools[tool_index];
                            if tool.done_emitted {
                                continue;
                            }
                            tool.ended = true;
                            let item_id = tool.item_id.clone();
                            let call_id = tool.call_id.clone();
                            let output_index = tool.output_index;
                            let arguments = tool.arguments.clone();
                            let arguments_complete = tool.arguments.trim().is_empty()
                                || serde_json::from_str::<Value>(&tool.arguments)
                                    .is_ok_and(|value| value.is_object());
                            let status = if complete && arguments_complete {
                                "completed"
                            } else {
                                "incomplete"
                            };
                            let input = custom_input(&arguments);
                            let (done_event, done_payload) = if tool.custom {
                                (
                                    "response.custom_tool_call_input.done",
                                    json!({"item_id":item_id,"call_id":call_id,"output_index":output_index,"input":input}),
                                )
                            } else {
                                (
                                    "response.function_call_arguments.done",
                                    json!({"item_id":item_id,"call_id":call_id,"output_index":output_index,"arguments":arguments}),
                                )
                            };
                            if tool.custom && !input.is_empty() {
                                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.custom_tool_call_input.delta", json!({"item_id":item_id,"call_id":call_id,"output_index":output_index,"delta":input})) {
                                    yield Ok(Event::default().event("response.custom_tool_call_input.delta").data(data.to_string()));
                                }
                            }
                            if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, done_event, done_payload) {
                                yield Ok(Event::default().event(done_event).data(data.to_string()));
                            }
                            let item = tool_item(ToolItemSpec {
                                id: &item_id,
                                call_id: &call_id,
                                name: &tool.name,
                                arguments: &tool.arguments,
                                status,
                                custom: tool.custom,
                                response_name: &tool.response_name,
                                namespace: tool.namespace.as_deref(),
                            });
                            if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.done", json!({"output_index":output_index,"item":item})) {
                                yield Ok(Event::default().event("response.output_item.done").data(data.to_string()));
                            }
                            tool.done_emitted = true;
                        }
                    }
                }
                GenerationEvent::ThinkingDelta { text } => {
                    live.thinking.push_str(&text);
                    let (reasoning_index, added) = live.ensure_reasoning();
                    let item_id = live.reasoning[reasoning_index].item_id.clone();
                    let output_index = live.reasoning[reasoning_index].output_index;
                    if added {
                        let item = reasoning_item(&live.reasoning[reasoning_index], "in_progress");
                        if let Ok(data) = attach_sequence(
                            &state,
                            &id,
                            store,
                            &mut sequence,
                            "response.output_item.added",
                            json!({"output_index":output_index,"item":item}),
                        ) {
                            yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                        }
                        if let Ok(data) = attach_sequence(
                            &state,
                            &id,
                            store,
                            &mut sequence,
                            "response.reasoning_summary_part.added",
                            json!({
                                "item_id":item_id.clone(),
                                "output_index":output_index,
                                "summary_index":0,
                                "part":{"type":"summary_text","text":""}
                            }),
                        ) {
                            yield Ok(Event::default().event("response.reasoning_summary_part.added").data(data.to_string()));
                        }
                    }
                    if !text.is_empty() {
                        live.reasoning[reasoning_index].summary.push_str(&text);
                        if let Ok(data) = attach_sequence(
                            &state,
                            &id,
                            store,
                            &mut sequence,
                            "response.reasoning_summary_text.delta",
                            json!({
                                "item_id":item_id,
                                "output_index":output_index,
                                "summary_index":0,
                                "delta":text
                            }),
                        ) {
                            yield Ok(Event::default().event("response.reasoning_summary_text.delta").data(data.to_string()));
                        }
                    }
                }
                GenerationEvent::Usage { usage } => live.usage = Some(usage),
                GenerationEvent::Stop { reason } => live.stop_reason = Some(reason),
                GenerationEvent::Error { .. } => unreachable!(),
            }
            refresh_snapshot(
                &snapshot,
                &internal.messages,
                &stored_tools,
                &live,
                &id,
                &model,
                created_at,
            );
        }
        if failed { return; }
        let tail = text_filter.finish();
        if !tail.is_empty() {
            let (item_id, output_index, added) = live.ensure_text();
            if added {
                let item = json!({"type":"message","id":item_id,"status":"in_progress","role":"assistant","content":[]});
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":output_index,"item":item})) {
                    yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.content_part.added", json!({"item_id":item_id,"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}})) {
                    yield Ok(Event::default().event("response.content_part.added").data(data.to_string()));
                }
            }
            live.text.push_str(&tail);
            if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_text.delta", json!({"item_id":item_id,"output_index":output_index,"content_index":0,"delta":tail,"logprobs":[]})) {
                yield Ok(Event::default().event("response.output_text.delta").data(data.to_string()));
            }
        }
        let response = accumulator.finish();
        tracing::debug!(
            protocol = "openai_responses",
            model = %model,
            stream_end_status = if response.incomplete { "incomplete" } else { "completed" },
            "OpenAI Responses stream completed"
        );
        let payload = responses_payload_with_live_items_and_tools(
            &id,
            &model,
            &stored_tools,
            &response,
            &live,
            created_at,
        );
        // Close every item in the same arrival order used for output_index.
        if live.item_order.is_empty() {
            if let Some(item) = payload["output"].as_array().and_then(|items| items.first()) {
                let item_id = item["id"].as_str().unwrap_or_default();
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":0,"item":item})) {
                    yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.content_part.added", json!({"item_id":item_id,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}})) {
                    yield Ok(Event::default().event("response.content_part.added").data(data.to_string()));
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_text.done", json!({"item_id":item_id,"output_index":0,"content_index":0,"text":"","logprobs":[]})) {
                    yield Ok(Event::default().event("response.output_text.done").data(data.to_string()));
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.content_part.done", json!({"item_id":item_id,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}})) {
                    yield Ok(Event::default().event("response.content_part.done").data(data.to_string()));
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.done", json!({"output_index":0,"item":item})) {
                    yield Ok(Event::default().event("response.output_item.done").data(data.to_string()));
                }
            }
        }
        for item in &live.item_order {
            match item {
                LiveItem::Reasoning(reasoning_index) => {
                    let reasoning = &live.reasoning[*reasoning_index];
                    if !reasoning.done_emitted {
                        let item_status =
                            if payload["status"] == "incomplete" { "incomplete" } else { "completed" };
                        if let Ok(data) = attach_sequence(
                            &state,
                            &id,
                            store,
                            &mut sequence,
                            "response.reasoning_summary_text.done",
                            json!({
                                "item_id":reasoning.item_id.clone(),
                                "output_index":reasoning.output_index,
                                "summary_index":0,
                                "text":reasoning.summary
                            }),
                        ) {
                            yield Ok(Event::default().event("response.reasoning_summary_text.done").data(data.to_string()));
                        }
                        if let Ok(data) = attach_sequence(
                            &state,
                            &id,
                            store,
                            &mut sequence,
                            "response.reasoning_summary_part.done",
                            json!({
                                "item_id":reasoning.item_id.clone(),
                                "output_index":reasoning.output_index,
                                "summary_index":0,
                                "part":{"type":"summary_text","text":reasoning.summary}
                            }),
                        ) {
                            yield Ok(Event::default().event("response.reasoning_summary_part.done").data(data.to_string()));
                        }
                        if let Some(item) = payload["output"]
                            .as_array()
                            .and_then(|items| items.iter().find(|item| item["id"] == reasoning.item_id))
                        {
                            let mut item = item.clone();
                            item["status"] = json!(item_status);
                            if let Ok(data) = attach_sequence(
                                &state,
                                &id,
                                store,
                                &mut sequence,
                                "response.output_item.done",
                                json!({"output_index":reasoning.output_index,"item":item}),
                            ) {
                                yield Ok(Event::default().event("response.output_item.done").data(data.to_string()));
                            }
                        }
                    }
                }
                LiveItem::Text => {
                    if let (Some(item_id), Some(output_index)) = (&live.text_item_id, live.text_output_index) {
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_text.done", json!({"item_id":item_id,"output_index":output_index,"content_index":0,"text":response.text,"logprobs":[]})) {
                            yield Ok(Event::default().event("response.output_text.done").data(data.to_string()));
                        }
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.content_part.done", json!({"item_id":item_id,"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":response.text,"annotations":[],"logprobs":[]}})) {
                            yield Ok(Event::default().event("response.content_part.done").data(data.to_string()));
                        }
                        if let Some(item) = payload["output"].as_array().and_then(|items| items.iter().find(|item| item["id"] == *item_id)) {
                            if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.done", json!({"output_index":output_index,"item":item})) {
                                yield Ok(Event::default().event("response.output_item.done").data(data.to_string()));
                            }
                        }
                    }
                }
                LiveItem::Tool(tool_index) => {
                    let tool = &live.tools[*tool_index];
                    if !tool.ended {
                        let arguments = response
                            .tool_calls
                            .iter()
                            .find(|call| call.id == tool.call_id)
                            .map(ToolCall::arguments_json)
                            .unwrap_or_default();
                        let input = custom_input(&arguments);
                        let (done_event, done_payload) = if tool.custom {
                            if !input.is_empty() {
                                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.custom_tool_call_input.delta", json!({"item_id":tool.item_id,"call_id":tool.call_id,"output_index":tool.output_index,"delta":input})) {
                                    yield Ok(Event::default().event("response.custom_tool_call_input.delta").data(data.to_string()));
                                }
                            }
                            (
                                "response.custom_tool_call_input.done",
                                json!({"item_id":tool.item_id,"call_id":tool.call_id,"output_index":tool.output_index,"input":input}),
                            )
                        } else {
                            (
                                "response.function_call_arguments.done",
                                json!({"item_id":tool.item_id,"call_id":tool.call_id,"output_index":tool.output_index,"arguments":arguments}),
                            )
                        };
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, done_event, done_payload) {
                            yield Ok(Event::default().event(done_event).data(data.to_string()));
                        }
                    }
                    if !tool.done_emitted {
                        if let Some(item) = payload["output"].as_array().and_then(|items| items.iter().find(|item| item["id"] == tool.item_id)) {
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.done", json!({"output_index":tool.output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.done").data(data.to_string()));
                        }
                        }
                    }
                }
            }
        }
        let terminal = if payload["status"] == "incomplete" { "response.incomplete" } else { "response.completed" };
        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, terminal, json!({"response":payload.clone()})) {
            yield Ok(Event::default().event(terminal).data(data.to_string()));
        }
        if let Some(record) = record {
            let status = if payload["status"] == "incomplete" { ResponseStatus::Incomplete } else { ResponseStatus::Completed };
            let messages = response_messages(&internal.messages, &response);
            let _ = state.responses.update(record, status, json!({"messages":messages,"tools":stored_tools,"response":payload}));
        }
        {
            let mut snapshot_state = snapshot.lock();
            snapshot_state.status = if payload["status"] == "incomplete" {
                ResponseStatus::Incomplete
            } else {
                ResponseStatus::Completed
            };
            snapshot_state.response = payload;
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn responses_payload_with_live_items_and_tools(
    id: &str,
    model: &str,
    tools: &[ToolDefinition],
    response: &GenerationResult,
    live: &LiveResponseState,
    created_at: i64,
) -> Value {
    let incomplete_reason = responses_incomplete_reason(response);
    let status = if incomplete_reason.is_some() { "incomplete" } else { "completed" };
    let mut output = Vec::new();
    for item in &live.item_order {
        match item {
            LiveItem::Reasoning(reasoning_index) => {
                if let Some(reasoning) = live.reasoning.get(*reasoning_index) {
                    output.push(reasoning_item(reasoning, status));
                }
            }
            LiveItem::Text => {
                if let Some(item_id) = &live.text_item_id {
                    output.push(json!({"type":"message","id":item_id,"status":status,"role":"assistant","content":[{"type":"output_text","text":response.text,"annotations":[],"logprobs":[]}]}));
                }
            }
            LiveItem::Tool(index) => {
                if let Some(tool) = live.tools.get(*index) {
                    if let Some(call) =
                        response.tool_calls.iter().find(|call| call.id == tool.call_id)
                    {
                        let custom = tool.custom || custom_tool_info(&call.name, tools).is_some();
                        let custom_info = custom_tool_info(&call.name, tools);
                        let response_name = custom_info
                            .as_ref()
                            .map(|info| info.name.as_str())
                            .unwrap_or(&tool.response_name);
                        let namespace = custom_info
                            .as_ref()
                            .and_then(|info| info.namespace.as_deref())
                            .or(tool.namespace.as_deref());
                        let arguments = call.arguments_json();
                        output.push(tool_item(ToolItemSpec {
                            id: &tool.item_id,
                            call_id: &tool.call_id,
                            name: &call.name,
                            arguments: &arguments,
                            status: if call.complete { "completed" } else { "incomplete" },
                            custom,
                            response_name,
                            namespace,
                        }));
                    }
                }
            }
        }
    }
    if output.is_empty() {
        output.push(json!({"type":"message","id":format!("msg_{}", uuid::Uuid::now_v7()),"status":status,"role":"assistant","content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]}));
    }
    json!({"id":id,"object":"response","created_at":created_at,"status":status,"error":null,"incomplete_details":incomplete_reason.map(|reason| json!({"reason":reason})),"model":model,"output":output,"output_text":response.text,"usage":response.usage})
}

fn refresh_snapshot(
    snapshot: &Arc<Mutex<ResponseSnapshot>>,
    input_messages: &[Message],
    tools: &[crate::generation::ToolDefinition],
    live: &LiveResponseState,
    id: &str,
    model: &str,
    created_at: i64,
) {
    let response = live.snapshot_response();
    let payload =
        responses_payload_with_live_items_and_tools(id, model, tools, &response, live, created_at);
    let mut state = snapshot.lock();
    state.messages = response_messages(input_messages, &response);
    state.tools = tools.to_vec();
    state.response = payload;
}
