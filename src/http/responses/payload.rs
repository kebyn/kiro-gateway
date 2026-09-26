use crate::{
    generation::{ContentPart, GenerationResult, Message, Role, ToolDefinition},
    transform::converter::responses_incomplete_reason,
};
use serde_json::{Value, json};

use super::{
    live::{ToolItemSpec, custom_tool_info, reasoning_item, tool_item},
    state::LiveReasoning,
};

pub(super) fn response_in_progress_payload(id: &str, model: &str) -> Value {
    response_in_progress_payload_at(id, model, chrono::Utc::now().timestamp())
}

pub(super) fn response_in_progress_payload_at(id: &str, model: &str, created_at: i64) -> Value {
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

pub(super) fn response_error_payload(id: &str, model: &str, message: &str) -> Value {
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

pub(super) fn response_failed_payload(id: &str, model: &str, message: &str) -> Value {
    let mut payload = response_error_payload(id, model, message);
    payload["status"] = json!("failed");
    payload["incomplete_details"] = Value::Null;
    payload
}

#[cfg(test)]
pub(super) fn responses_payload(id: &str, model: &str, response: &GenerationResult) -> Value {
    responses_payload_with_tools(id, model, &[], response)
}

pub(super) fn responses_payload_with_tools(
    id: &str,
    model: &str,
    tools: &[ToolDefinition],
    response: &GenerationResult,
) -> Value {
    let incomplete_reason = responses_incomplete_reason(response);
    let status = if incomplete_reason.is_some() { "incomplete" } else { "completed" };
    let mut output = Vec::new();
    if !response.thinking.is_empty() {
        let reasoning = LiveReasoning {
            item_id: format!("rs_{}", uuid::Uuid::now_v7()),
            output_index: 0,
            summary: response.thinking.clone(),
            done_emitted: true,
        };
        output.push(reasoning_item(&reasoning, status));
    }
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
        let arguments = call.arguments_json();
        output.push(tool_item(ToolItemSpec {
            id: &item_id,
            call_id: &call.id,
            name: &call.name,
            arguments: &arguments,
            status: if call.complete { "completed" } else { "incomplete" },
            custom: custom.is_some(),
            response_name,
            namespace,
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

pub(super) fn response_messages(input: &[Message], response: &GenerationResult) -> Vec<Message> {
    let mut messages = input.to_vec();
    if !response.text.is_empty() || !response.thinking.is_empty() || !response.tool_calls.is_empty()
    {
        let mut content = Vec::new();
        if !response.thinking.is_empty() {
            content.push(ContentPart::thinking(response.thinking.clone()));
        }
        if !response.text.is_empty() {
            content.push(ContentPart::text(response.text.clone()));
        }
        let mut assistant = Message::new(Role::Assistant, content);
        assistant.tool_calls = response.tool_calls.clone();
        messages.push(assistant);
    }
    messages
}
