use crate::{
    AppState,
    error::{AppError, Protocol, protocol_error_response},
    protocol::{
        internal::{
            InternalEvent, InternalMessage, InternalResponse, InternalTool, InternalToolCall, Usage,
        },
        openai_responses::ResponsesRequest,
    },
    response_store::{ResponseStatus, ResponseStore},
    transform::converter::responses_incomplete_reason,
    transform::truncation::XmlLeakFilter,
    upstream::request::InternalEventAccumulator,
};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{collections::HashMap, convert::Infallible, sync::Arc};

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
    let mut internal = body.into_internal(previous);
    internal
        .messages
        .iter()
        .try_for_each(|message| message.validate_content().map_err(AppError::BadRequest))?;
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
    let stored_messages = response_messages(&internal.messages, &InternalResponse::default());
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

fn response_in_progress_payload(id: &str, model: &str) -> Value {
    response_in_progress_payload_at(id, model, chrono::Utc::now().timestamp())
}

fn response_in_progress_payload_at(id: &str, model: &str, created_at: i64) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": "in_progress",
        "error": null,
        "incomplete_details": null,
        "model": model,
        "output": [],
        "output_text": "",
        "usage": null
    })
}

fn response_error_payload(id: &str, model: &str, message: &str) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "incomplete",
        "error": {"code":"upstream_error","message":message},
        "incomplete_details": {"reason":"error"},
        "model": model,
        "output": [],
        "output_text": "",
        "usage": null
    })
}

fn response_failed_payload(id: &str, model: &str, message: &str) -> Value {
    let mut payload = response_error_payload(id, model, message);
    payload["status"] = json!("failed");
    payload["incomplete_details"] = Value::Null;
    payload
}

#[derive(Clone, Debug)]
struct LiveTool {
    call_id: String,
    item_id: String,
    name: String,
    arguments: String,
    output_index: usize,
    ended: bool,
    done_emitted: bool,
    custom: bool,
    response_name: String,
    namespace: Option<String>,
}

#[derive(Clone, Debug)]
struct CustomToolInfo {
    name: String,
    namespace: Option<String>,
}

#[derive(Clone, Debug)]
enum LiveItem {
    Text,
    Tool(usize),
}

#[derive(Default)]
struct LiveResponseState {
    text_item_id: Option<String>,
    text_output_index: Option<usize>,
    tools: Vec<LiveTool>,
    tool_indices: HashMap<String, usize>,
    item_order: Vec<LiveItem>,
    next_output_index: usize,
    text: String,
    thinking: String,
    stop_reason: Option<String>,
    usage: Option<Usage>,
}

#[derive(Clone)]
struct ResponseSnapshot {
    messages: Vec<InternalMessage>,
    tools: Vec<crate::protocol::internal::InternalTool>,
    response: Value,
    status: ResponseStatus,
}

struct IncompleteRecordGuard {
    store: Option<ResponseStore>,
    record_id: String,
    snapshot: Arc<Mutex<ResponseSnapshot>>,
}

impl Drop for IncompleteRecordGuard {
    fn drop(&mut self) {
        let Some(store) = self.store.as_ref() else { return };
        let Ok(Some(record)) = store.get(&self.record_id) else { return };
        if record.status != ResponseStatus::InProgress {
            return;
        }
        let snapshot = self.snapshot.lock().clone();
        if snapshot.status != ResponseStatus::InProgress {
            return;
        }
        let mut payload = snapshot.response;
        payload["status"] = json!("incomplete");
        payload["error"] = json!({"code":"client_disconnected","message":"client disconnected before response completion"});
        payload["incomplete_details"] = json!({"reason":"client_disconnect"});
        let _ = store.update(
            record,
            ResponseStatus::Incomplete,
            json!({"messages":snapshot.messages,"tools":snapshot.tools,"response":payload}),
        );
        let _ = store.append_event(&self.record_id, "response.incomplete", &payload);
    }
}

impl LiveResponseState {
    fn snapshot_response(&self) -> InternalResponse {
        InternalResponse {
            text: self.text.clone(),
            thinking: self.thinking.clone(),
            tool_calls: self
                .tools
                .iter()
                .map(|tool| InternalToolCall {
                    id: tool.call_id.clone(),
                    name: tool.name.clone(),
                    arguments: serde_json::from_str(&tool.arguments)
                        .unwrap_or_else(|_| Value::String(tool.arguments.clone())),
                    complete: tool.ended
                        && (tool.arguments.trim().is_empty()
                            || serde_json::from_str::<Value>(&tool.arguments)
                                .is_ok_and(|value| value.is_object())),
                })
                .collect(),
            usage: self.usage.clone(),
            stop_reason: self.stop_reason.clone(),
            incomplete: self.stop_reason.is_none(),
        }
    }

    fn ensure_text(&mut self) -> (String, usize, bool) {
        if let (Some(id), Some(index)) = (&self.text_item_id, self.text_output_index) {
            return (id.clone(), index, false);
        }
        let id = format!("msg_{}", uuid::Uuid::now_v7());
        let index = self.next_output_index;
        self.next_output_index += 1;
        self.text_item_id = Some(id.clone());
        self.text_output_index = Some(index);
        self.item_order.push(LiveItem::Text);
        (id, index, true)
    }

    fn ensure_tool(
        &mut self,
        call_id: &str,
        name: &str,
        custom: Option<CustomToolInfo>,
    ) -> (usize, bool) {
        if let Some(index) = self.tool_indices.get(call_id).copied() {
            if !name.is_empty() && self.tools[index].name.is_empty() {
                self.tools[index].name = name.to_owned();
            }
            if let Some(custom) = custom {
                self.tools[index].custom = true;
                self.tools[index].response_name = custom.name;
                self.tools[index].namespace = custom.namespace;
            }
            return (index, false);
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let index = self.tools.len();
        let custom_flag = custom.is_some();
        let response_name =
            custom.as_ref().map(|tool| tool.name.clone()).unwrap_or_else(|| name.to_owned());
        let namespace = custom.and_then(|tool| tool.namespace);
        self.tools.push(LiveTool {
            call_id: call_id.to_owned(),
            item_id: format!("{}_{}", if custom_flag { "ctc" } else { "fc" }, uuid::Uuid::now_v7()),
            name: name.to_owned(),
            arguments: String::new(),
            output_index,
            ended: false,
            done_emitted: false,
            custom: custom_flag,
            response_name,
            namespace,
        });
        self.tool_indices.insert(call_id.to_owned(), index);
        self.item_order.push(LiveItem::Tool(index));
        (index, true)
    }
}

fn custom_tool_info(name: &str, tools: &[InternalTool]) -> Option<CustomToolInfo> {
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

fn tool_item(
    id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
    status: &str,
    custom: bool,
    response_name: &str,
    namespace: Option<&str>,
) -> Value {
    if custom {
        custom_item(id, call_id, response_name, namespace, &custom_input(arguments), status)
    } else {
        json!({
            "type":"function_call",
            "id":id,
            "call_id":call_id,
            "name":name,
            "arguments":arguments,
            "status":status
        })
    }
}

fn attach_sequence(
    state: &AppState,
    response_id: &str,
    store: bool,
    sequence: &mut u64,
    event_type: &'static str,
    mut payload: Value,
) -> Result<Value, AppError> {
    payload["type"] = Value::String(event_type.to_owned());
    let assigned = if store {
        state.responses.append_event(response_id, event_type, &payload)?.sequence_number
    } else {
        *sequence
    };
    payload["sequence_number"] = json!(assigned);
    *sequence = assigned.saturating_add(1);
    Ok(payload)
}

fn responses_live_stream(
    state: AppState,
    mut upstream: crate::upstream::request::InternalEventStream,
    id: String,
    model: String,
    internal: crate::protocol::internal::InternalRequest,
    store: bool,
    record: Option<crate::response_store::ResponseRecord>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let stored_messages = response_messages(&internal.messages, &InternalResponse::default());
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
        let mut accumulator = InternalEventAccumulator::new();
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
            if let InternalEvent::Error { message } = &event {
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
                InternalEvent::TextDelta { text } => {
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
                InternalEvent::ToolCallStart { id: call_id, name } => {
                    let key = if call_id.is_empty() { format!("tool_call_{}", live.tools.len() + 1) } else { call_id };
                    let custom = custom_tool_info(&name, &stored_tools);
                    let (tool_index, added) = live.ensure_tool(&key, &name, custom);
                    if added {
                        let tool = &live.tools[tool_index];
                        let item = tool_item(
                            &tool.item_id,
                            &tool.call_id,
                            &tool.name,
                            "",
                            "in_progress",
                            tool.custom,
                            &tool.response_name,
                            tool.namespace.as_deref(),
                        );
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":tool.output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                        }
                    }
                }
                InternalEvent::ToolCallDelta { id: call_id, arguments, name } => {
                    let key = if call_id.is_empty() { live.tools.last().map(|tool| tool.call_id.clone()).unwrap_or_else(|| format!("tool_call_{}", live.tools.len() + 1)) } else { call_id };
                    let tool_name = name.as_deref().unwrap_or_default();
                    let custom = custom_tool_info(tool_name, &stored_tools);
                    let (tool_index, added) = live.ensure_tool(&key, tool_name, custom);
                    if added {
                        let tool = &live.tools[tool_index];
                        let item = tool_item(
                            &tool.item_id,
                            &tool.call_id,
                            &tool.name,
                            "",
                            "in_progress",
                            tool.custom,
                            &tool.response_name,
                            tool.namespace.as_deref(),
                        );
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
                InternalEvent::ToolCallEnd { id: call_id, complete } => {
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
                            let item = tool_item(
                                &item_id,
                                &call_id,
                                &tool.name,
                                &tool.arguments,
                                status,
                                tool.custom,
                                &tool.response_name,
                                tool.namespace.as_deref(),
                            );
                            if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.done", json!({"output_index":output_index,"item":item})) {
                                yield Ok(Event::default().event("response.output_item.done").data(data.to_string()));
                            }
                            tool.done_emitted = true;
                        }
                    }
                }
                InternalEvent::ThinkingDelta { text } => live.thinking.push_str(&text),
                InternalEvent::Usage { usage } => live.usage = Some(usage),
                InternalEvent::Stop { reason } => live.stop_reason = Some(reason),
                InternalEvent::Error { .. } => unreachable!(),
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
                            .map(InternalToolCall::arguments_json)
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
    tools: &[InternalTool],
    response: &InternalResponse,
    live: &LiveResponseState,
    created_at: i64,
) -> Value {
    let incomplete_reason = responses_incomplete_reason(response);
    let status = if incomplete_reason.is_some() { "incomplete" } else { "completed" };
    let mut output = Vec::new();
    for item in &live.item_order {
        match item {
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
                        output.push(tool_item(
                            &tool.item_id,
                            &tool.call_id,
                            &call.name,
                            &call.arguments_json(),
                            if call.complete { "completed" } else { "incomplete" },
                            custom,
                            response_name,
                            namespace,
                        ));
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
    input_messages: &[InternalMessage],
    tools: &[crate::protocol::internal::InternalTool],
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

#[cfg(test)]
fn responses_payload(id: &str, model: &str, response: &InternalResponse) -> Value {
    responses_payload_with_tools(id, model, &[], response)
}

fn responses_payload_with_tools(
    id: &str,
    model: &str,
    tools: &[InternalTool],
    response: &InternalResponse,
) -> Value {
    let incomplete_reason = responses_incomplete_reason(response);
    let status = if incomplete_reason.is_some() { "incomplete" } else { "completed" };
    let mut output = Vec::new();
    if !response.text.is_empty() {
        output.push(json!({
            "type":"message",
            "id":format!("msg_{}", uuid::Uuid::now_v7()),
            "status":status,
            "role":"assistant",
            "content":[{
                "type":"output_text",
                "text":response.text,
                "annotations":[],
                "logprobs":[]
            }]
        }));
    }
    for call in &response.tool_calls {
        let custom = custom_tool_info(&call.name, tools);
        let item_id =
            format!("{}_{}", if custom.is_some() { "ctc" } else { "fc" }, uuid::Uuid::now_v7());
        let response_name = custom.as_ref().map(|info| info.name.as_str()).unwrap_or(&call.name);
        let namespace = custom.as_ref().and_then(|info| info.namespace.as_deref());
        output.push(tool_item(
            &item_id,
            &call.id,
            &call.name,
            &call.arguments_json(),
            if call.complete { "completed" } else { "incomplete" },
            custom.is_some(),
            response_name,
            namespace,
        ));
    }
    if output.is_empty() {
        output.push(json!({
            "type":"message",
            "id":format!("msg_{}", uuid::Uuid::now_v7()),
            "status":status,
            "role":"assistant",
            "content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]
        }));
    }
    json!({
        "id":id,
        "object":"response",
        "created_at":chrono::Utc::now().timestamp(),
        "status":status,
        "error":null,
        "incomplete_details":incomplete_reason.map(|reason| json!({"reason":reason})),
        "model":model,
        "output":output,
        "output_text":response.text,
        "usage":response.usage
    })
}

fn response_messages(
    input: &[InternalMessage],
    response: &InternalResponse,
) -> Vec<InternalMessage> {
    let mut messages = input.to_vec();
    if !response.text.is_empty() || !response.tool_calls.is_empty() {
        let mut assistant = InternalMessage::new(
            "assistant",
            if response.text.is_empty() {
                Value::Null
            } else {
                Value::String(response.text.clone())
            },
        );
        assistant.tool_calls = response.tool_calls.clone();
        messages.push(assistant);
    }
    messages
}

fn responses_stream_events(payload: &Value) -> Vec<(&'static str, Value)> {
    let mut events = Vec::new();
    let mut sequence_number = 0_u64;
    let mut created = payload.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["output_text"] = json!("");
    push_event(
        &mut events,
        &mut sequence_number,
        "response.created",
        json!({"response":created.clone()}),
    );
    push_event(
        &mut events,
        &mut sequence_number,
        "response.in_progress",
        json!({"response":created}),
    );

    for (output_index, item) in payload["output"].as_array().into_iter().flatten().enumerate() {
        match item["type"].as_str() {
            Some("message") => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["content"] = json!([]);
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.added",
                    json!({"output_index":output_index,"item":added}),
                );
                let part = item["content"][0].clone();
                let mut empty_part = part.clone();
                empty_part["text"] = json!("");
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.content_part.added",
                    json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"part":empty_part}),
                );
                if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                    push_event(
                        &mut events,
                        &mut sequence_number,
                        "response.output_text.delta",
                        json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"delta":text,"logprobs":[]}),
                    );
                }
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_text.done",
                    json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"text":part["text"],"logprobs":[]}),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.content_part.done",
                    json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"part":part}),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.done",
                    json!({"output_index":output_index,"item":item}),
                );
            }
            Some("function_call") => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["arguments"] = json!("");
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.added",
                    json!({"output_index":output_index,"item":added}),
                );
                let arguments = item["arguments"].as_str().unwrap_or_default();
                if !arguments.is_empty() {
                    push_event(
                        &mut events,
                        &mut sequence_number,
                        "response.function_call_arguments.delta",
                        json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"delta":arguments}),
                    );
                }
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.function_call_arguments.done",
                    json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"arguments":arguments}),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.done",
                    json!({"output_index":output_index,"item":item}),
                );
            }
            Some("custom_tool_call") => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["input"] = json!("");
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.added",
                    json!({"output_index":output_index,"item":added}),
                );
                let input = item["input"].as_str().unwrap_or_default();
                if !input.is_empty() {
                    push_event(
                        &mut events,
                        &mut sequence_number,
                        "response.custom_tool_call_input.delta",
                        json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"delta":input}),
                    );
                }
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.custom_tool_call_input.done",
                    json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"input":input}),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.done",
                    json!({"output_index":output_index,"item":item}),
                );
            }
            _ => {}
        }
    }
    let terminal = if payload["status"] == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    push_event(&mut events, &mut sequence_number, terminal, json!({"response":payload}));
    events
}

fn push_event(
    events: &mut Vec<(&'static str, Value)>,
    sequence_number: &mut u64,
    event_type: &'static str,
    mut data: Value,
) {
    data["type"] = Value::String(event_type.into());
    data["sequence_number"] = json!(*sequence_number);
    events.push((event_type, data));
    *sequence_number += 1;
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

#[cfg(test)]
mod tests {
    use super::{
        create, response_messages, responses_live_stream, responses_payload,
        responses_payload_with_tools, responses_stream_events,
    };
    use crate::{
        AppState,
        app_state::build_upstream,
        auth::{AuthMethod, Credential, SecretString},
        config::AppConfig,
        credential::TokenManager,
        model_catalog::ModelInfo,
        protocol::internal::{InternalMessage, InternalResponse, InternalTool, InternalToolCall},
        protocol::openai_responses::ResponsesRequest,
        response_store::{ResponseStatus, ResponseStore},
    };
    use axum::response::IntoResponse;
    use axum::{Json, extract::State};
    use futures_util::stream;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::{path::Path, sync::Arc};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn response(text: &str) -> InternalResponse {
        InternalResponse {
            text: text.into(),
            tool_calls: vec![InternalToolCall {
                id: "call_upstream".into(),
                name: "lookup".into(),
                arguments: json!({"id":42}),
                complete: true,
            }],
            ..Default::default()
        }
    }

    fn state(path: &Path) -> AppState {
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            response_store_path: path.display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let token_manager = Arc::new(TokenManager::new(&config, credential).unwrap());
        token_manager.seed_models_for_tests(vec![ModelInfo {
            model_id: "kiro".into(),
            model_name: None,
            description: None,
            token_limits: None,
        }]);
        let client = reqwest::Client::new();
        AppState {
            config: Arc::new(config.clone()),
            token_manager,
            responses: ResponseStore::open(path).unwrap(),
            upstream: build_upstream(&config, client),
            sessions: Default::default(),
        }
    }

    async fn state_with_json_upstream(path: &Path) -> (AppState, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = br#"{"content":"answer","usage":{"inputTokens":1,"outputTokens":1},"stopReason":"end_turn"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            response_store_path: path.display().to_string(),
            upstream_url: Some(format!("http://{address}")),
            ..Default::default()
        };
        let credential = Credential {
            auth_method: AuthMethod::ApiKey,
            access_token: Some(SecretString::new("token")),
            ..Default::default()
        };
        let token_manager = Arc::new(TokenManager::new(&config, credential).unwrap());
        token_manager.seed_models_for_tests(vec![ModelInfo {
            model_id: "kiro".into(),
            model_name: None,
            description: None,
            token_limits: None,
        }]);
        let client = reqwest::Client::new();
        (
            AppState {
                config: Arc::new(config.clone()),
                token_manager,
                responses: ResponseStore::open(path).unwrap(),
                upstream: build_upstream(&config, client),
                sessions: Default::default(),
            },
            server,
        )
    }

    #[tokio::test]
    async fn live_stream_emits_incremental_events_and_store_false_leaves_no_record() {
        let directory = tempfile::tempdir().unwrap();
        let (state, server) =
            state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"reasoning","id":"rs_stream","summary":[],"encrypted_content":"opaque-reasoning"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}
            ],
            "store":false,
            "stream":true
        }))
        .unwrap();
        let response = create(State(state.clone()), Json(request)).await.unwrap();
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        server.await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("response.created"));
        assert!(body.contains("response.output_text.delta"));
        assert!(body.contains("response.completed"));
        let id = body
            .lines()
            .find(|line| line.starts_with("data: ") && line.contains("response.created"))
            .and_then(|line| serde_json::from_str::<Value>(line.trim_start_matches("data: ")).ok())
            .and_then(|value| value["response"]["id"].as_str().map(ToOwned::to_owned))
            .unwrap();
        assert!(state.responses.get(&id).unwrap().is_none());
    }

    #[tokio::test]
    async fn non_stream_returns_openai_response_payload() {
        let directory = tempfile::tempdir().unwrap();
        let (state, server) =
            state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":"hello",
            "store":false,
            "stream":false
        }))
        .unwrap();
        let response = create(State(state), Json(request)).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        server.await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["object"], "response");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["output_text"], "answer");
    }

    #[tokio::test]
    async fn non_stream_accepts_codex_reasoning_history() {
        let directory = tempfile::tempdir().unwrap();
        let (state, server) =
            state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {
                    "type":"reasoning",
                    "id":"rs_1",
                    "summary":[],
                    "encrypted_content":"opaque-reasoning"
                },
                {
                    "type":"message",
                    "role":"user",
                    "content":[{"type":"input_text","text":"hello"}]
                }
            ],
            "store":false,
            "stream":false
        }))
        .unwrap();

        let response = create(State(state), Json(request)).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        server.await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "completed");
        assert_eq!(body["output_text"], "answer");
    }

    #[tokio::test]
    async fn stored_live_stream_persists_ordered_events_and_final_response() {
        let directory = tempfile::tempdir().unwrap();
        let (state, server) =
            state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":"hello",
            "store":true,
            "stream":true
        }))
        .unwrap();
        let response = create(State(state.clone()), Json(request)).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        server.await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let id = body
            .lines()
            .find(|line| line.starts_with("data: ") && line.contains("response.created"))
            .and_then(|line| serde_json::from_str::<Value>(line.trim_start_matches("data: ")).ok())
            .and_then(|value| value["response"]["id"].as_str().map(ToOwned::to_owned))
            .unwrap();
        let events = state.responses.events(&id).unwrap();
        assert!(events.len() >= 5);
        assert_eq!(events.first().unwrap().event_type, "response.created");
        assert!(events.iter().any(|event| event.event_type == "response.output_text.delta"));
        assert!(events.last().unwrap().event_type == "response.completed");
        assert!(events.windows(2).all(|pair| pair[0].sequence_number < pair[1].sequence_number));
        let stored = state.responses.get(&id).unwrap().unwrap();
        assert_eq!(stored.status, ResponseStatus::Completed);
        assert!(state.responses.delete(&id).unwrap());
        assert!(state.responses.events(&id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_tool_arguments_and_item_completion_keep_arrival_order() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(&directory.path().join("responses.sqlite3"));
        let internal = crate::protocol::internal::InternalRequest {
            model: "kiro".into(),
            messages: vec![InternalMessage::new("user", Value::String("lookup".into()))],
            system: None,
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            max_tokens: None,
            temperature: None,
            conversation_id: None,
            instructions: None,
        };
        let upstream: crate::upstream::request::InternalEventStream = Box::pin(stream::iter(vec![
            Ok(crate::protocol::internal::InternalEvent::ToolCallStart {
                id: "call_1".into(),
                name: "lookup".into(),
            }),
            Ok(crate::protocol::internal::InternalEvent::ToolCallDelta {
                id: "call_1".into(),
                arguments: "{\"x\":".into(),
                name: None,
            }),
            Ok(crate::protocol::internal::InternalEvent::ToolCallDelta {
                id: "call_1".into(),
                arguments: "1}".into(),
                name: None,
            }),
            Ok(crate::protocol::internal::InternalEvent::ToolCallEnd {
                id: "call_1".into(),
                complete: true,
            }),
            Ok(crate::protocol::internal::InternalEvent::Stop { reason: "end_turn".into() }),
        ]));
        let response = responses_live_stream(
            state,
            upstream,
            "resp_test".into(),
            "kiro".into(),
            internal,
            false,
            None,
        )
        .into_response();
        let body =
            String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec())
                .unwrap();
        let delta = body.find("response.function_call_arguments.delta").unwrap();
        let done = body.find("response.function_call_arguments.done").unwrap();
        let item_done = body.find("response.output_item.done").unwrap();
        assert!(delta < done && done < item_done);
        assert!(body.contains("\\\"x\\\":1}"));
        assert!(body.contains("response.completed"));
    }

    #[tokio::test]
    async fn live_custom_tool_emits_custom_input_events_and_output_item() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(&directory.path().join("responses.sqlite3"));
        let internal = crate::protocol::internal::InternalRequest {
            model: "kiro".into(),
            messages: vec![InternalMessage::new("user", Value::String("apply patch".into()))],
            system: None,
            tools: vec![InternalTool {
                name: "functions_apply_patch".into(),
                description: Some("Apply a patch".into()),
                input_schema: json!({"type":"object"}),
                custom: true,
                original_name: Some("apply_patch".into()),
                namespace: Some("functions".into()),
            }],
            tool_choice: None,
            stream: true,
            max_tokens: None,
            temperature: None,
            conversation_id: None,
            instructions: None,
        };
        let upstream: crate::upstream::request::InternalEventStream = Box::pin(stream::iter(vec![
            Ok(crate::protocol::internal::InternalEvent::ToolCallStart {
                id: "call_custom".into(),
                name: "functions_apply_patch".into(),
            }),
            Ok(crate::protocol::internal::InternalEvent::ToolCallDelta {
                id: "call_custom".into(),
                arguments: "{\"input\":\"*** Begin\\n+hello\\n*** End\"}".into(),
                name: None,
            }),
            Ok(crate::protocol::internal::InternalEvent::ToolCallEnd {
                id: "call_custom".into(),
                complete: true,
            }),
            Ok(crate::protocol::internal::InternalEvent::Stop { reason: "end_turn".into() }),
        ]));
        let response = responses_live_stream(
            state,
            upstream,
            "resp_custom".into(),
            "kiro".into(),
            internal,
            false,
            None,
        )
        .into_response();
        let body =
            String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec())
                .unwrap();
        assert!(body.contains("response.custom_tool_call_input.delta"));
        assert!(body.contains("response.custom_tool_call_input.done"));
        assert!(body.contains("\"type\":\"custom_tool_call\""));
        assert!(body.contains("\"name\":\"apply_patch\""));
        assert!(body.contains("\"namespace\":\"functions\""));
        assert!(body.contains("*** Begin"));
    }

    #[tokio::test]
    async fn dropping_a_live_stream_marks_an_in_progress_record_incomplete() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(&directory.path().join("responses.sqlite3"));
        let record = state
            .responses
            .create_with_id(
                "resp_disconnect".into(),
                "kiro",
                json!({"messages":[],"tools":[],"response":{}}),
                ResponseStatus::InProgress,
            )
            .unwrap();
        let internal = crate::protocol::internal::InternalRequest {
            model: "kiro".into(),
            messages: Vec::new(),
            system: None,
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            max_tokens: None,
            temperature: None,
            conversation_id: None,
            instructions: None,
        };
        let upstream: crate::upstream::request::InternalEventStream = Box::pin(stream::pending());
        let response = responses_live_stream(
            state.clone(),
            upstream,
            record.id.clone(),
            "kiro".into(),
            internal,
            true,
            Some(record),
        )
        .into_response();
        drop(response);
        assert_eq!(
            state.responses.get("resp_disconnect").unwrap().unwrap().status,
            ResponseStatus::Incomplete
        );
        assert!(
            state
                .responses
                .events("resp_disconnect")
                .unwrap()
                .iter()
                .any(|event| event.event_type == "response.incomplete")
        );
    }

    #[tokio::test]
    async fn missing_previous_response_is_not_treated_as_empty_history() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(&directory.path().join("responses.sqlite3"));
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":"hello",
            "previous_response_id":"resp_missing"
        }))
        .unwrap();
        assert!(matches!(
            create(State(state), Json(request)).await,
            Err(crate::error::AppError::NotFound)
        ));
    }

    #[test]
    fn function_call_item_has_distinct_item_and_call_ids() {
        let payload = responses_payload("resp_test", "kiro", &response("answer"));
        let call = &payload["output"][1];
        assert!(call["id"].as_str().unwrap().starts_with("fc_"));
        assert_ne!(call["id"], call["call_id"]);
        assert_eq!(call["call_id"], "call_upstream");
        assert_eq!(call["arguments"], r#"{"id":42}"#);
        assert_eq!(call["status"], "completed");
    }

    #[test]
    fn custom_tool_response_restores_name_namespace_and_freeform_input() {
        let tools = vec![InternalTool {
            name: "functions_apply_patch".into(),
            description: Some("Apply a patch".into()),
            input_schema: json!({"type":"object"}),
            custom: true,
            original_name: Some("apply_patch".into()),
            namespace: Some("functions".into()),
        }];
        let response = InternalResponse {
            tool_calls: vec![InternalToolCall {
                id: "call_custom".into(),
                name: "functions_apply_patch".into(),
                arguments: json!({"input":"*** Begin\n+hello\n*** End"}),
                complete: true,
            }],
            ..Default::default()
        };
        let payload = responses_payload_with_tools("resp_test", "kiro", &tools, &response);
        let call = &payload["output"][0];
        assert_eq!(call["type"], "custom_tool_call");
        assert!(call["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(call["call_id"], "call_custom");
        assert_eq!(call["name"], "apply_patch");
        assert_eq!(call["namespace"], "functions");
        assert_eq!(call["input"], "*** Begin\n+hello\n*** End");

        let events = responses_stream_events(&payload);
        assert!(events.iter().any(|(kind, _)| *kind == "response.custom_tool_call_input.delta"));
        assert!(events.iter().any(|(kind, _)| *kind == "response.custom_tool_call_input.done"));
    }

    #[test]
    fn stream_orders_items_before_final_response() {
        let payload = responses_payload("resp_test", "kiro", &response("answer"));
        let events = responses_stream_events(&payload);
        let types: Vec<_> = events.iter().map(|event| event.0).collect();
        assert_eq!(types.first(), Some(&"response.created"));
        assert_eq!(types.get(1), Some(&"response.in_progress"));
        assert_eq!(types.last(), Some(&"response.completed"));
        let call_added = types
            .iter()
            .position(|kind| *kind == "response.function_call_arguments.delta")
            .unwrap();
        let call_done =
            types.iter().position(|kind| *kind == "response.function_call_arguments.done").unwrap();
        let item_done =
            types.iter().rposition(|kind| *kind == "response.output_item.done").unwrap();
        assert!(call_added < call_done && call_done < item_done && item_done < types.len() - 1);
        let call_event = &events[call_added].1;
        assert_eq!(call_event["output_index"], 1);
        assert_eq!(call_event["item_id"], payload["output"][1]["id"]);
        assert_eq!(events.last().unwrap().1["response"], payload);
        for (index, (_, event)) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], index as u64);
        }
    }

    #[test]
    fn tool_only_stream_has_no_text_delta_and_can_finish_incomplete() {
        let mut response = response("");
        response.incomplete = true;
        response.tool_calls[0].complete = false;
        let payload = responses_payload("resp_test", "kiro", &response);
        let events = responses_stream_events(&payload);
        assert!(!events.iter().any(|event| event.0 == "response.output_text.delta"));
        assert_eq!(events[2].1["output_index"], 0);
        assert_eq!(events.last().unwrap().0, "response.incomplete");
        assert_eq!(events.last().unwrap().1["response"]["status"], "incomplete");
    }

    #[test]
    fn incomplete_payload_exposes_reason_and_stream_call_identity() {
        let mut response = response("");
        response.stop_reason = Some("MAX_TOKENS".into());
        response.incomplete = true;
        let payload = responses_payload("resp_test", "kiro", &response);
        assert_eq!(payload["status"], "incomplete");
        assert_eq!(payload["incomplete_details"]["reason"], "max_output_tokens");
        let events = responses_stream_events(&payload);
        let delta = events
            .iter()
            .find(|(kind, _)| *kind == "response.function_call_arguments.delta")
            .unwrap();
        assert_eq!(delta.1["call_id"], "call_upstream");
        assert_eq!(delta.1["item_id"], payload["output"][0]["id"]);
        assert_eq!(delta.1["output_index"], 0);
    }

    #[test]
    fn empty_response_has_a_message_output_item() {
        let payload = responses_payload("resp_test", "kiro", &InternalResponse::default());
        assert_eq!(payload["output"].as_array().unwrap().len(), 1);
        assert_eq!(payload["output"][0]["type"], "message");
        assert_eq!(payload["output"][0]["content"][0]["text"], "");
    }

    #[test]
    fn stored_response_replays_tool_call_for_previous_response_id() {
        let input = vec![InternalMessage::new("user", Value::String("question".into()))];
        let messages = response_messages(&input, &response(""));
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record =
            store.create("kiro", json!({"messages":messages}), ResponseStatus::Completed).unwrap();
        let replayed = ResponseStore::extract_messages(&store.get(&record.id).unwrap().unwrap());
        let continuation: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[{
                "type":"function_call_output",
                "call_id":"call_upstream",
                "output":"found"
            }],
            "previous_response_id":record.id
        }))
        .unwrap();
        let internal = continuation.into_internal(replayed);
        assert_eq!(internal.messages.len(), 3);
        assert_eq!(internal.messages[1].tool_calls[0].id, "call_upstream");
        assert_eq!(internal.messages[1].tool_calls[0].arguments["id"], 42);
        assert_eq!(internal.messages[2].tool_results[0].tool_call_id, "call_upstream");
        assert_eq!(internal.messages[2].tool_results[0].content, "found");
    }

    #[test]
    fn stored_response_replays_tool_definitions_for_continuation() {
        use crate::protocol::internal::InternalTool;
        let input = vec![InternalMessage::new("user", Value::String("question".into()))];
        let messages = response_messages(&input, &response(""));
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record = store
            .create(
                "kiro",
                json!({
                    "messages":messages,
                    "tools":[InternalTool { name:"lookup".into(), description:None, input_schema:json!({"type":"object"}), custom:false, original_name:None, namespace:None }]
                }),
                ResponseStatus::Completed,
            )
            .unwrap();
        let recovered = ResponseStore::extract_tools(&store.get(&record.id).unwrap().unwrap());
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].name, "lookup");
    }
}
