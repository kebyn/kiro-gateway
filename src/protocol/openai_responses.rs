use crate::protocol::internal::{InternalMessage, InternalRequest, InternalTool};
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
    pub parallel_tool_calls: Option<bool>,
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
        if let Some(items) = self.input.as_array() {
            for item in items {
                if let Some(message) = parse_item(item) {
                    messages.push(message);
                }
            }
        } else {
            messages.push(InternalMessage {
                role: "user".into(),
                content: self.input,
                name: None,
                tool_call_id: None,
            });
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

fn parse_item(item: &Value) -> Option<InternalMessage> {
    let role = item.get("role").and_then(Value::as_str)?.to_owned();
    let content =
        item.get("content").cloned().or_else(|| item.get("text").cloned()).unwrap_or(Value::Null);
    Some(InternalMessage {
        role,
        content,
        name: item.get("name").and_then(Value::as_str).map(ToOwned::to_owned),
        tool_call_id: item
            .get("call_id")
            .or_else(|| item.get("tool_call_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}
fn parse_tool(tool: Value) -> Option<InternalTool> {
    let function = tool.get("function").unwrap_or(&tool);
    Some(InternalTool {
        name: function.get("name")?.as_str()?.to_owned(),
        description: function.get("description").and_then(Value::as_str).map(ToOwned::to_owned),
        input_schema: function
            .get("parameters")
            .or_else(|| function.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object"})),
    })
}
