use crate::{
    AppState,
    error::AppError,
    protocol::{
        internal::{InternalEvent, InternalMessage, InternalResponse, InternalToolCall},
        openai_responses::ResponsesRequest,
    },
    response_store::{ResponseStatus, ResponseStore},
    transform::converter::responses_incomplete_reason,
    transform::truncation::XmlLeakFilter,
    upstream::request::InternalEventAccumulator,
};
use axum::{
    Json,
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::HashMap, convert::Infallible};

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<ResponsesRequest>,
) -> Result<Response, AppError> {
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
    if internal.tools.is_empty() {
        internal.tools = previous_tools;
    }
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
                    let failed = response_error_payload(&id, &model, &error.to_string());
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
                let failed = response_error_payload(&id, &model, &error.to_string());
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
    let payload = responses_payload(&id, &model, &response);
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
    json!({
        "id": id,
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
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

#[derive(Clone, Debug)]
struct LiveTool {
    call_id: String,
    item_id: String,
    name: String,
    arguments: String,
    output_index: usize,
    ended: bool,
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
}

impl LiveResponseState {
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

    fn ensure_tool(&mut self, call_id: &str, name: &str) -> (usize, bool) {
        if let Some(index) = self.tool_indices.get(call_id).copied() {
            if !name.is_empty() && self.tools[index].name.is_empty() {
                self.tools[index].name = name.to_owned();
            }
            return (index, false);
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let index = self.tools.len();
        self.tools.push(LiveTool {
            call_id: call_id.to_owned(),
            item_id: format!("fc_{}", uuid::Uuid::now_v7()),
            name: name.to_owned(),
            arguments: String::new(),
            output_index,
            ended: false,
        });
        self.tool_indices.insert(call_id.to_owned(), index);
        self.item_order.push(LiveItem::Tool(index));
        (index, true)
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
    let stream = async_stream::stream! {
        let mut sequence = 0_u64;
        let mut live = LiveResponseState::default();
        let mut accumulator = InternalEventAccumulator::new();
        let mut text_filter = XmlLeakFilter::new();
        let mut failed = false;
        let initial = response_in_progress_payload(&id, &model);
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
                    let payload = response_error_payload(&id, &model, &error.to_string());
                    if store {
                        if let Some(record) = record.clone() {
                            let _ = state.responses.update(record, ResponseStatus::Failed, json!({"messages":stored_messages,"tools":stored_tools,"response":payload}));
                        }
                        let _ = state.responses.append_event(&id, "response.failed", &payload);
                    }
                    match attach_sequence(&state, &id, store, &mut sequence, "response.incomplete", json!({"response":payload,"error":{"code":"upstream_error","message":error.to_string()}})) {
                        Ok(data) => yield Ok(Event::default().event("response.incomplete").data(data.to_string())),
                        Err(_) => {}
                    }
                    failed = true;
                    break;
                }
            };
            if let InternalEvent::Error { message } = &event {
                let payload = response_error_payload(&id, &model, message);
                if store {
                    if let Some(record) = record.clone() {
                        let _ = state.responses.update(record, ResponseStatus::Failed, json!({"messages":stored_messages,"tools":stored_tools,"response":payload}));
                    }
                    let _ = state.responses.append_event(&id, "response.failed", &payload);
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.incomplete", json!({"response":payload,"error":{"code":"upstream_error","message":message}})) {
                    yield Ok(Event::default().event("response.incomplete").data(data.to_string()));
                }
                failed = true;
                break;
            }
            if let Err(error) = accumulator.push(event.clone()) {
                let payload = response_error_payload(&id, &model, &error.to_string());
                if store {
                    if let Some(record) = record.clone() {
                        let _ = state.responses.update(record, ResponseStatus::Failed, json!({"messages":stored_messages,"tools":stored_tools,"response":payload}));
                    }
                    let _ = state.responses.append_event(&id, "response.failed", &payload);
                }
                if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.incomplete", json!({"response":payload,"error":{"code":"upstream_error","message":error.to_string()}})) {
                    yield Ok(Event::default().event("response.incomplete").data(data.to_string()));
                }
                failed = true;
                break;
            }
            match event {
                InternalEvent::TextDelta { text } => {
                    let text = text_filter.push(&text);
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
                    let (tool_index, added) = live.ensure_tool(&key, &name);
                    if added {
                        let tool = &live.tools[tool_index];
                        let item = json!({"type":"function_call","id":tool.item_id,"call_id":tool.call_id,"name":tool.name,"arguments":"","status":"in_progress"});
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":tool.output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                        }
                    }
                }
                InternalEvent::ToolCallDelta { id: call_id, arguments, name } => {
                    let key = if call_id.is_empty() { live.tools.last().map(|tool| tool.call_id.clone()).unwrap_or_else(|| format!("tool_call_{}", live.tools.len() + 1)) } else { call_id };
                    let (tool_index, added) = live.ensure_tool(&key, name.as_deref().unwrap_or_default());
                    if added {
                        let tool = &live.tools[tool_index];
                        let item = json!({"type":"function_call","id":tool.item_id,"call_id":tool.call_id,"name":tool.name,"arguments":"","status":"in_progress"});
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.added", json!({"output_index":tool.output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.added").data(data.to_string()));
                        }
                    }
                    if !arguments.is_empty() {
                        live.tools[tool_index].arguments.push_str(&arguments);
                    }
                    if !arguments.is_empty() {
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
                            tool.ended = true;
                            let item_id = tool.item_id.clone();
                            let call_id = tool.call_id.clone();
                            let output_index = tool.output_index;
                            let arguments = tool.arguments.clone();
                            let _ = complete;
                            if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.function_call_arguments.done", json!({"item_id":item_id,"call_id":call_id,"output_index":output_index,"arguments":arguments})) {
                                yield Ok(Event::default().event("response.function_call_arguments.done").data(data.to_string()));
                            }
                        }
                    }
                }
                InternalEvent::ThinkingDelta { .. } | InternalEvent::Usage { .. } | InternalEvent::Stop { .. } => {}
                InternalEvent::Error { .. } => unreachable!(),
            }
        }
        if failed { return; }
        let response = accumulator.finish();
        let payload = responses_payload_with_live_items(&id, &model, &response, &live);
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
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.function_call_arguments.done", json!({"item_id":tool.item_id,"call_id":tool.call_id,"output_index":tool.output_index,"arguments":response.tool_calls.iter().find(|call| call.id == tool.call_id).map(InternalToolCall::arguments_json).unwrap_or_default()})) {
                            yield Ok(Event::default().event("response.function_call_arguments.done").data(data.to_string()));
                        }
                    }
                    if let Some(item) = payload["output"].as_array().and_then(|items| items.iter().find(|item| item["id"] == tool.item_id)) {
                        if let Ok(data) = attach_sequence(&state, &id, store, &mut sequence, "response.output_item.done", json!({"output_index":tool.output_index,"item":item})) {
                            yield Ok(Event::default().event("response.output_item.done").data(data.to_string()));
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
    };
    Sse::new(stream)
}

fn responses_payload_with_live_items(
    id: &str,
    model: &str,
    response: &InternalResponse,
    live: &LiveResponseState,
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
                        output.push(json!({"type":"function_call","id":tool.item_id,"call_id":tool.call_id,"name":call.name,"arguments":call.arguments_json(),"status":if call.complete {"completed"} else {"incomplete"}}));
                    }
                }
            }
        }
    }
    if output.is_empty() {
        output.push(json!({"type":"message","id":format!("msg_{}", uuid::Uuid::now_v7()),"status":status,"role":"assistant","content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]}));
    }
    json!({"id":id,"object":"response","created_at":chrono::Utc::now().timestamp(),"status":status,"error":null,"incomplete_details":incomplete_reason.map(|reason| json!({"reason":reason})),"model":model,"output":output,"output_text":response.text,"usage":response.usage})
}

fn responses_payload(id: &str, model: &str, response: &InternalResponse) -> Value {
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
        output.push(json!({
            "type":"function_call",
            "id":format!("fc_{}", uuid::Uuid::now_v7()),
            "call_id":call.id,
            "name":call.name,
            "arguments":call.arguments_json(),
            "status":if call.complete { "completed" } else { "incomplete" }
        }));
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
    use super::{create, response_messages, responses_payload, responses_stream_events};
    use crate::{
        AppState,
        app_state::build_upstream,
        auth::{AuthMethod, Credential},
        config::AppConfig,
        credential::TokenManager,
        protocol::internal::{InternalMessage, InternalResponse, InternalToolCall},
        protocol::openai_responses::ResponsesRequest,
        response_store::{ResponseStatus, ResponseStore},
    };
    use axum::{Json, extract::State};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::{path::Path, sync::Arc};

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
        let client = reqwest::Client::new();
        AppState {
            config: Arc::new(config.clone()),
            token_manager,
            responses: ResponseStore::open(path).unwrap(),
            upstream: build_upstream(&config, client),
            sessions: Default::default(),
        }
    }

    #[tokio::test]
    async fn live_stream_emits_incremental_events_and_store_false_leaves_no_record() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(&directory.path().join("responses.sqlite3"));
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":"hello",
            "store":false,
            "stream":true
        }))
        .unwrap();
        let response = create(State(state.clone()), Json(request)).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
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
    async fn stored_live_stream_persists_ordered_events_and_final_response() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(&directory.path().join("responses.sqlite3"));
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":"hello",
            "store":true,
            "stream":true
        }))
        .unwrap();
        let response = create(State(state.clone()), Json(request)).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
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
                    "tools":[InternalTool { name:"lookup".into(), description:None, input_schema:json!({"type":"object"}) }]
                }),
                ResponseStatus::Completed,
            )
            .unwrap();
        let recovered = ResponseStore::extract_tools(&store.get(&record.id).unwrap().unwrap());
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].name, "lookup");
    }
}
