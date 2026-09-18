use crate::protocol::internal::{InternalMessage, InternalRequest, InternalTool};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub tools: Vec<AnthropicTool>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
}
#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Value,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "input_schema")]
    pub input_schema: Value,
}
#[derive(Debug, Deserialize)]
pub struct CountTokensRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub tools: Vec<AnthropicTool>,
}
#[derive(Debug, Serialize)]
pub struct CountTokensResponse {
    pub input_tokens: u64,
}

impl From<MessagesRequest> for InternalRequest {
    fn from(value: MessagesRequest) -> Self {
        Self {
            model: value.model,
            messages: value
                .messages
                .into_iter()
                .map(|message| InternalMessage {
                    role: message.role,
                    content: message.content,
                    name: None,
                    tool_call_id: None,
                })
                .collect(),
            system: value.system.map(|v| text_value(&v)),
            tools: value
                .tools
                .into_iter()
                .map(|tool| InternalTool {
                    name: tool.name,
                    description: tool.description,
                    input_schema: tool.input_schema,
                })
                .collect(),
            tool_choice: value.tool_choice,
            stream: value.stream,
            max_tokens: value.max_tokens,
            temperature: value.temperature,
            conversation_id: None,
            instructions: None,
        }
    }
}
fn text_value(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.to_string(),
    }
}
