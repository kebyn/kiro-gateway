use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}
impl Usage {
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self { input_tokens, output_tokens, total_tokens: input_tokens + output_tokens }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InternalMessage {
    pub role: String,
    pub content: Value,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<InternalToolCall>,
    #[serde(default)]
    pub tool_results: Vec<InternalToolResult>,
}

impl InternalMessage {
    pub fn new(role: impl Into<String>, content: Value) -> Self {
        Self {
            role: role.into(),
            content,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InternalTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InternalRequest {
    pub model: String,
    pub messages: Vec<InternalMessage>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub tools: Vec<InternalTool>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

impl InternalRequest {
    pub fn last_user_text(&self) -> String {
        self.messages.iter().rev().find(|m| m.role == "user").map(content_text).unwrap_or_default()
    }
    pub fn input_text(&self) -> String {
        self.messages.iter().map(content_text).collect::<Vec<_>>().join("\n")
    }
}

pub fn content_text(message: &InternalMessage) -> String {
    value_text(&message.content)
}

pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(items) => items.iter().map(value_text).collect::<Vec<_>>().join(""),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("output_text"))
            .or_else(|| object.get("input_text"))
            .map(value_text)
            .unwrap_or_default(),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InternalEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta {
        id: String,
        arguments: String,
        #[serde(default)]
        name: Option<String>,
    },
    ToolCallEnd {
        id: String,
        complete: bool,
    },
    Usage {
        usage: super::internal::Usage,
    },
    Stop {
        reason: String,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InternalResponse {
    pub text: String,
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub tool_calls: Vec<InternalToolCall>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub incomplete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InternalToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default)]
    pub complete: bool,
}

impl InternalToolCall {
    pub fn arguments_json(&self) -> String {
        match &self.arguments {
            Value::String(arguments) => arguments.clone(),
            arguments => arguments.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InternalToolResult {
    pub tool_call_id: String,
    pub content: Value,
    #[serde(default)]
    pub is_error: bool,
}
