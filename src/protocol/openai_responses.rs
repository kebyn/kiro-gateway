use crate::protocol::internal::{
    InternalMessage, InternalRequest, InternalTool, InternalToolCall, InternalToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    #[serde(rename = "parallel_tool_calls")]
    pub _parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub conversation: Option<String>,
    #[serde(default = "default_true")]
    pub store: bool,
    #[serde(default)]
    pub stream: bool,
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct ResponsesError {
    pub code: String,
    pub message: String,
}

impl ResponsesRequest {
    pub fn into_internal(self, previous: Vec<InternalMessage>) -> InternalRequest {
        let mut messages = previous;
        let previous_len = messages.len();
        match self.input {
            Value::Array(items) => {
                for item in items {
                    append_item(&mut messages, &item, previous_len);
                }
            }
            Value::String(text) => messages.push(InternalMessage::new("user", Value::String(text))),
            item @ Value::Object(_) => {
                if !append_item(&mut messages, &item, previous_len) {
                    messages.push(InternalMessage::new("user", item));
                }
            }
            value => messages.push(InternalMessage::new("user", value)),
        }
        let tools = self.tools.into_iter().filter_map(parse_tool).collect();
        InternalRequest {
            model: self.model,
            messages,
            system: None,
            tools,
            tool_choice: self.tool_choice,
            stream: self.stream,
            max_tokens: None,
            temperature: None,
            conversation_id: self.conversation.or(self.previous_response_id),
            instructions: self.instructions,
        }
    }
}

fn append_item(messages: &mut Vec<InternalMessage>, item: &Value, previous_len: usize) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let Some(call) = parse_function_call(item) else {
                return false;
            };
            let can_merge = messages.len() > previous_len;
            if let Some(message) =
                messages.last_mut().filter(|message| can_merge && message.role == "assistant")
            {
                message.tool_calls.push(call);
            } else {
                let mut message = InternalMessage::new("assistant", Value::Null);
                message.tool_calls.push(call);
                messages.push(message);
            }
            true
        }
        Some("function_call_output") => {
            let Some(tool_call_id) = item.get("call_id").and_then(Value::as_str) else {
                return false;
            };
            let mut message = InternalMessage::new("tool", Value::Null);
            message.tool_call_id = Some(tool_call_id.to_owned());
            message.tool_results.push(InternalToolResult {
                tool_call_id: tool_call_id.to_owned(),
                content: item.get("output").cloned().unwrap_or(Value::Null),
                is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || item.get("status").and_then(Value::as_str) == Some("failed"),
            });
            messages.push(message);
            true
        }
        Some("message") if item.get("role").and_then(Value::as_str).is_some() => {
            let role = item.get("role").and_then(Value::as_str).unwrap();
            let Some(content) = item.get("content").cloned() else {
                return false;
            };
            let mut message = InternalMessage::new(role, content);
            message.name = item.get("name").and_then(Value::as_str).map(ToOwned::to_owned);
            messages.push(message);
            true
        }
        _ => false,
    }
}

fn parse_function_call(item: &Value) -> Option<InternalToolCall> {
    let id = item.get("call_id")?.as_str()?.to_owned();
    let name = item.get("name")?.as_str()?.to_owned();
    let (arguments, arguments_complete) =
        parse_arguments(item.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({})));
    let status_complete = !matches!(
        item.get("status").and_then(Value::as_str),
        Some("incomplete" | "failed" | "cancelled")
    );
    Some(InternalToolCall { id, name, arguments, complete: arguments_complete && status_complete })
}

fn parse_arguments(arguments: Value) -> (Value, bool) {
    match arguments {
        Value::String(arguments) if arguments.trim().is_empty() => {
            (Value::Object(Default::default()), true)
        }
        Value::String(arguments) => match serde_json::from_str::<Value>(&arguments) {
            Ok(value) if value.is_object() => (value, true),
            Ok(value) => (value, false),
            Err(_) => (Value::String(arguments), false),
        },
        Value::Object(object) => (Value::Object(object), true),
        value => (value, false),
    }
}
fn parse_tool(tool: Value) -> Option<InternalTool> {
    Some(InternalTool {
        name: tool.get("name")?.as_str()?.to_owned(),
        description: tool.get("description").and_then(Value::as_str).map(ToOwned::to_owned),
        input_schema: tool
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object"})),
    })
}

#[cfg(test)]
mod tests {
    use super::ResponsesRequest;
    use crate::protocol::internal::InternalMessage;
    use serde_json::{Value, json};

    #[test]
    fn parses_parallel_calls_and_function_outputs() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking"}]},
                {"type":"function_call","id":"fc_item_a","call_id":"call_a","name":"alpha","arguments":"{\"value\":1}"},
                {"type":"function_call","id":"fc_item_b","call_id":"call_b","name":"beta","arguments":"{\"value\":2}"},
                {"type":"function_call_output","call_id":"call_a","output":"first"},
                {"type":"function_call_output","call_id":"call_b","output":{"answer":2},"status":"failed"}
            ]
        }))
        .unwrap();

        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 3);
        assert_eq!(internal.messages[0].tool_calls.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_a");
        assert_eq!(internal.messages[0].tool_calls[1].arguments["value"], 2);
        assert_eq!(internal.messages[1].tool_results[0].content, Value::String("first".into()));
        assert!(internal.messages[2].tool_results[0].is_error);
    }

    #[test]
    fn appends_function_output_to_stored_tool_call_history() {
        let mut assistant = InternalMessage::new("assistant", Value::Null);
        assistant.tool_calls.push(crate::protocol::internal::InternalToolCall {
            id: "call_saved".into(),
            name: "lookup".into(),
            arguments: json!({"id":7}),
            complete: true,
        });
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[{"type":"function_call_output","call_id":"call_saved","output":"done"}]
        }))
        .unwrap();

        let internal = request.into_internal(vec![assistant]);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_saved");
        assert_eq!(internal.messages[1].tool_results[0].tool_call_id, "call_saved");
    }

    #[test]
    fn merges_consecutive_function_calls_and_marks_invalid_arguments_incomplete() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"function_call","id":"fc_a","call_id":"call_a","name":"alpha","arguments":"{\"a\":1}"},
                {"type":"function_call","id":"fc_b","call_id":"call_b","name":"beta","arguments":"{\"b\":"},
                {"type":"message","role":"user","content":"after calls"},
                {"type":"function_call","id":"fc_c","call_id":"call_c","name":"gamma","arguments":{},"status":"incomplete"}
            ]
        }))
        .unwrap();
        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 3);
        assert_eq!(internal.messages[0].tool_calls.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_a");
        assert_eq!(internal.messages[0].tool_calls[1].id, "call_b");
        assert!(!internal.messages[0].tool_calls[1].complete);
        assert_eq!(internal.messages[1].role, "user");
        assert_eq!(internal.messages[1].content, "after calls");
        assert_eq!(internal.messages[2].tool_calls[0].id, "call_c");
        assert!(!internal.messages[2].tool_calls[0].complete);
    }

    #[test]
    fn rejects_function_call_without_call_id() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[{"type":"function_call","id":"fc_only","name":"lookup","arguments":{}}]
        }))
        .unwrap();
        let internal = request.into_internal(Vec::new());
        assert!(internal.messages.is_empty() || internal.messages[0].tool_calls.is_empty());
    }

    #[test]
    fn does_not_merge_first_new_call_into_previous_response_assistant() {
        let mut previous = InternalMessage::new("assistant", Value::Null);
        previous.tool_calls.push(crate::protocol::internal::InternalToolCall {
            id: "call_previous".into(),
            name: "old".into(),
            arguments: json!({}),
            complete: true,
        });
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"function_call","call_id":"call_new_a","name":"alpha","arguments":{}},
                {"type":"function_call","call_id":"call_new_b","name":"beta","arguments":{}}
            ]
        }))
        .unwrap();
        let internal = request.into_internal(vec![previous]);
        assert_eq!(internal.messages.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_previous");
        assert_eq!(internal.messages[1].tool_calls.len(), 2);
        assert_eq!(internal.messages[1].tool_calls[0].id, "call_new_a");
        assert_eq!(internal.messages[1].tool_calls[1].id, "call_new_b");
    }
}
