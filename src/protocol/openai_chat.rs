use crate::protocol::internal::{InternalMessage, InternalRequest, InternalTool};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<ChatTool>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}
#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<Value>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<Value>,
}
#[derive(Debug, Deserialize)]
pub struct ChatTool {
    pub r#type: String,
    pub function: ChatFunction,
}
#[derive(Debug, Deserialize)]
pub struct ChatFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Value,
}

impl From<ChatRequest> for InternalRequest {
    fn from(value: ChatRequest) -> Self {
        Self {
            model: value.model,
            messages: value
                .messages
                .into_iter()
                .map(|m| InternalMessage {
                    role: m.role,
                    content: m.content.unwrap_or(Value::Null),
                    name: m.name,
                    tool_call_id: m.tool_call_id,
                })
                .collect(),
            system: None,
            tools: value
                .tools
                .into_iter()
                .filter(|t| t.r#type == "function")
                .map(|t| InternalTool {
                    name: t.function.name,
                    description: t.function.description,
                    input_schema: t.function.parameters,
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
