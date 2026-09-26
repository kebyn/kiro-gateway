use std::collections::HashSet;

use serde_json::Value;

use crate::{
    error::AppError,
    generation::{GenerationEvent, ToolCall, Usage},
};

const MAX_JSON_DEPTH: usize = 8;
const MAX_JSON_NODES: usize = 512;

/// Converts a complete JSON response into the same logical events produced by
/// the binary EventStream endpoint. JSON is necessarily finite, but downstream
/// protocol handlers still consume it through the one event path.
pub(crate) fn decode_events(body: &[u8]) -> Result<Vec<GenerationEvent>, AppError> {
    let body: Value = serde_json::from_slice(body)
        .map_err(|error| AppError::Upstream(format!("invalid JSON upstream response: {error}")))?;
    let mut nodes = Vec::new();
    collect_nodes(&body, &mut nodes, 0).map_err(|error| AppError::Integrity(error.into()))?;
    tracing::debug!(shape = %shape(&body), "received JSON upstream response shape");
    if let Some(error) = body.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .unwrap_or("upstream error")
            .to_owned();
        return Ok(vec![GenerationEvent::Error { message }]);
    }
    if let Some(message) = error_message(&nodes) {
        return Ok(vec![GenerationEvent::Error { message }]);
    }
    let mut events = Vec::new();
    let mut recognized = has_explicit_empty_response(&nodes);
    if let Some(text) =
        nodes.iter().find_map(|node| text(node, &["content", "text", "output_text", "outputText"]))
    {
        if !text.is_empty() {
            events.push(GenerationEvent::TextDelta { text });
        }
        recognized = true;
    }
    if let Some(text) =
        nodes.iter().find_map(|node| text(node, &["thinking", "reasoning", "reasoningText"]))
    {
        if !text.is_empty() {
            events.push(GenerationEvent::ThinkingDelta { text });
        }
        recognized = true;
    }
    let mut seen_tool_calls = HashSet::new();
    for node in &nodes {
        for call in parse_tool_calls(node) {
            let dedupe_key = if call.id.is_empty() {
                format!("{}:{}", call.name, call.arguments_json())
            } else {
                call.id.clone()
            };
            if !seen_tool_calls.insert(dedupe_key) {
                continue;
            }
            recognized = true;
            let id = call.id.clone();
            let name = call.name.clone();
            let arguments = call.arguments_json();
            let complete = call.complete;
            events.push(GenerationEvent::ToolCallStart { id: id.clone(), name });
            events.push(GenerationEvent::ToolCallDelta { id: id.clone(), arguments, name: None });
            events.push(GenerationEvent::ToolCallEnd { id, complete });
        }
    }
    if let Some(usage) = nodes.iter().find_map(|node| node.get("usage")) {
        events.push(GenerationEvent::Usage {
            usage: Usage::new(
                usage
                    .get("inputTokens")
                    .or_else(|| usage.get("input_tokens"))
                    .or_else(|| usage.get("prompt_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                usage
                    .get("outputTokens")
                    .or_else(|| usage.get("output_tokens"))
                    .or_else(|| usage.get("completion_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            ),
        });
        recognized = true;
    }
    let reason = nodes
        .iter()
        .find_map(|node| {
            ["stopReason", "stop_reason", "finish_reason", "finishReason"]
                .iter()
                .find_map(|name| node.get(*name).and_then(Value::as_str))
        })
        .map(ToOwned::to_owned);
    if reason.is_some() {
        recognized = true;
    }
    if !recognized {
        return Err(AppError::Integrity(format!(
            "unrecognized JSON upstream response shape: {}",
            shape(&body)
        )));
    }
    events.push(GenerationEvent::Stop { reason: reason.as_deref().unwrap_or("end_turn").into() });
    Ok(events)
}

fn text(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        let value = value.get(*name)?;
        match value {
            Value::String(text) => Some(text.clone()),
            Value::Object(object) => object
                .get("text")
                .or_else(|| object.get("outputText"))
                .or_else(|| object.get("content"))
                .and_then(value_text),
            Value::Array(items) => {
                let text = items.iter().filter_map(value_text).collect::<Vec<_>>().join("");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        }
    })
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => ["text", "output_text", "outputText", "content"]
            .iter()
            .find_map(|name| object.get(*name).and_then(value_text)),
        Value::Array(items) => {
            let text = items.iter().filter_map(value_text).collect::<Vec<_>>().join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn error_message(nodes: &[&Value]) -> Option<String> {
    nodes.iter().find_map(|node| {
        let object = node.as_object()?;
        let error_type = object.get("__type").and_then(Value::as_str)?;
        let message = object.get("message").and_then(Value::as_str)?;
        if message.is_empty() {
            return None;
        }
        Some(format!("{error_type}: {message}"))
    })
}

fn collect_nodes<'a>(
    value: &'a Value,
    nodes: &mut Vec<&'a Value>,
    depth: usize,
) -> Result<(), &'static str> {
    if depth > MAX_JSON_DEPTH {
        return Err("upstream JSON nesting exceeds configured depth");
    }
    if nodes.len() >= MAX_JSON_NODES {
        return Err("upstream JSON contains too many nodes");
    }
    nodes.push(value);
    match value {
        Value::Object(object) => {
            for child in object.values() {
                collect_nodes(child, nodes, depth + 1)?;
            }
        }
        Value::Array(items) => {
            for child in items.iter().take(64) {
                collect_nodes(child, nodes, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn has_explicit_empty_response(nodes: &[&Value]) -> bool {
    nodes.iter().any(|node| {
        let Some(object) = node.as_object() else {
            return false;
        };
        let empty_content =
            ["content", "output"].iter().filter_map(|name| object.get(*name)).any(|value| {
                match value {
                    Value::String(text) => text.is_empty(),
                    Value::Array(items) => items.is_empty(),
                    _ => false,
                }
            });
        let completed_status = object
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "complete" | "completed" | "succeeded"));
        empty_content || completed_status
    })
}

fn shape(value: &Value) -> String {
    fn render(value: &Value, depth: usize) -> String {
        if depth >= 3 {
            return match value {
                Value::Array(_) => "array".into(),
                Value::Object(_) => "object".into(),
                Value::String(_) => "string".into(),
                Value::Number(_) => "number".into(),
                Value::Bool(_) => "bool".into(),
                Value::Null => "null".into(),
            };
        }
        match value {
            Value::Object(object) => {
                let mut keys = object.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                let fields = keys
                    .into_iter()
                    .take(16)
                    .map(|key| format!("{key}:{}", render(&object[&key], depth + 1)))
                    .collect::<Vec<_>>();
                let suffix = (object.len() > 16).then_some(",...").unwrap_or_default();
                format!("object{{{}{suffix}}}", fields.join(","))
            }
            Value::Array(items) => {
                let shapes =
                    items.iter().take(4).map(|item| render(item, depth + 1)).collect::<Vec<_>>();
                let suffix = (items.len() > 4).then_some(",...").unwrap_or_default();
                format!("array[{}{suffix}]", shapes.join(","))
            }
            Value::String(_) => "string".into(),
            Value::Number(_) => "number".into(),
            Value::Bool(_) => "bool".into(),
            Value::Null => "null".into(),
        }
    }
    render(value, 0)
}

pub(crate) fn parse_tool_calls(body: &Value) -> Vec<ToolCall> {
    let mut items = Vec::new();
    for key in ["toolUses", "tool_uses", "toolUse", "toolCalls", "tool_calls", "toolCall", "output"]
    {
        match body.get(key) {
            Some(Value::Array(values)) => items.extend(values.iter()),
            Some(value @ Value::Object(_)) => items.push(value),
            _ => {}
        }
    }
    if items.is_empty()
        && body.get("name").is_some()
        && (body.get("input").is_some() || body.get("arguments").is_some())
    {
        items.push(body);
    }
    items
        .iter()
        .filter_map(|item| {
            if item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| !matches!(kind, "function_call" | "tool_use" | "function"))
            {
                return None;
            }
            let id = item
                .get("toolUseId")
                .or_else(|| item.get("tool_use_id"))
                .or_else(|| item.get("call_id"))
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)?
                .to_owned();
            let name = item
                .get("name")
                .or_else(|| item.get("toolName"))
                .or_else(|| item.get("function").and_then(|function| function.get("name")))
                .and_then(Value::as_str)?
                .to_owned();
            let arguments = item
                .get("input")
                .or_else(|| item.get("arguments"))
                .or_else(|| item.get("function").and_then(|function| function.get("arguments")))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let (arguments, arguments_complete) = match arguments {
                Value::String(raw) if raw.trim().is_empty() => (serde_json::json!({}), true),
                Value::String(raw) => match serde_json::from_str::<Value>(&raw) {
                    Ok(value) if value.is_object() => (value, true),
                    Ok(value) => (value, false),
                    Err(_) => (Value::String(raw), false),
                },
                Value::Object(object) => (Value::Object(object), true),
                value => (value, false),
            };
            let complete = item.get("complete").and_then(Value::as_bool).unwrap_or(true)
                && arguments_complete
                && !matches!(
                    item.get("status").and_then(Value::as_str),
                    Some("incomplete" | "failed" | "cancelled")
                );
            Some(ToolCall { id, name, arguments, complete })
        })
        .collect()
}
