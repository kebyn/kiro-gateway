use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
    Developer,
}

impl Role {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            "system" => Ok(Self::System),
            "developer" => Ok(Self::Developer),
            other => Err(format!("unsupported message role: {other}")),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::System => "system",
            Self::Developer => "developer",
        }
    }

    pub const fn is_instruction(self) -> bool {
        matches!(self, Self::System | Self::Developer)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<&str> for Role {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    Thinking { text: String },
}

impl ContentPart {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text { text: value.into() }
    }

    pub fn thinking(value: impl Into<String>) -> Self {
        Self::Thinking { text: value.into() }
    }

    pub fn as_text(&self) -> &str {
        match self {
            Self::Text { text } | Self::Thinking { text } => text,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default)]
    pub complete: bool,
}

impl ToolCall {
    pub fn arguments_json(&self) -> String {
        match &self.arguments {
            Value::String(arguments) => arguments.clone(),
            arguments => arguments.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: Value,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_results: Vec<ToolResult>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self {
            role,
            content,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    pub fn empty(role: Role) -> Self {
        Self::new(role, Vec::new())
    }

    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![ContentPart::text(text)])
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default)]
    pub custom: bool,
    #[serde(default)]
    pub original_name: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OpaqueHistory {
    pub item_type: String,
    pub payload: Value,
}

impl OpaqueHistory {
    pub fn from_item(item: Value) -> Result<Self, String> {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "opaque history item requires type".to_owned())?
            .to_owned();
        Ok(Self { item_type, payload: item })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
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
    #[serde(default)]
    pub opaque_history: Vec<OpaqueHistory>,
}

impl GenerationRequest {
    pub fn input_text(&self) -> String {
        self.messages.iter().map(content_text).collect::<Vec<_>>().join("\n")
    }
}

pub fn content_text(message: &Message) -> String {
    message.content.iter().map(ContentPart::as_text).collect::<Vec<_>>().join("")
}

pub fn value_text(value: &Value) -> String {
    let mut nodes = 0;
    value_text_bounded(value, 0, &mut nodes)
}

const MAX_VALUE_DEPTH: usize = 8;
const MAX_VALUE_NODES: usize = 256;

fn value_text_bounded(value: &Value, depth: usize, nodes: &mut usize) -> String {
    if depth > MAX_VALUE_DEPTH || *nodes >= MAX_VALUE_NODES {
        return String::new();
    }
    *nodes += 1;
    match value {
        Value::String(value) => value.clone(),
        Value::Array(items) => items
            .iter()
            .take(MAX_VALUE_NODES)
            .map(|item| value_text_bounded(item, depth + 1, nodes))
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("output_text"))
            .or_else(|| object.get("input_text"))
            .map(|value| value_text_bounded(value, depth + 1, nodes))
            .unwrap_or_default(),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ContextWindowExceeded,
    Refusal,
    StopSequence,
    PauseTurn,
    StreamIncomplete,
    Unknown(String),
}

impl StopReason {
    pub fn from_upstream(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        match reason.trim().to_ascii_lowercase().as_str() {
            "end_turn" | "complete" | "completed" | "stop" => Self::EndTurn,
            "max_tokens" | "max_output_tokens" | "length" => Self::MaxTokens,
            "model_context_window_exceeded" | "context_window_exceeded" | "context_limit" => {
                Self::ContextWindowExceeded
            }
            "refusal" | "content_filter" | "content_filtered" | "guardrail_intervened" => {
                Self::Refusal
            }
            "stop_sequence" => Self::StopSequence,
            "pause_turn" => Self::PauseTurn,
            "stream_incomplete" | "incomplete" | "upstream_disconnect" => Self::StreamIncomplete,
            _ => Self::Unknown(reason),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::ContextWindowExceeded => "context_window_exceeded",
            Self::Refusal => "refusal",
            Self::StopSequence => "stop_sequence",
            Self::PauseTurn => "pause_turn",
            Self::StreamIncomplete => "stream_incomplete",
            Self::Unknown(reason) => reason,
        }
    }

    pub const fn is_incomplete(&self) -> bool {
        matches!(self, Self::StreamIncomplete)
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<String> for StopReason {
    fn from(value: String) -> Self {
        Self::from_upstream(value)
    }
}

impl From<&str> for StopReason {
    fn from(value: &str) -> Self {
        Self::from_upstream(value)
    }
}

impl PartialEq<&str> for StopReason {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<str> for StopReason {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl Serialize for StopReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_upstream)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GenerationEvent {
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
        usage: Usage,
    },
    Stop {
        reason: StopReason,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenerationResult {
    pub text: String,
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub incomplete: bool,
}

#[cfg(test)]
mod tests {
    use super::{ContentPart, Message, Role, StopReason, content_text};

    #[test]
    fn message_content_is_bounded_to_known_domain_variants() {
        let message = Message::new(
            Role::Assistant,
            vec![ContentPart::text("hello"), ContentPart::thinking("hmm")],
        );
        assert_eq!(content_text(&message), "hellohmm");
    }

    #[test]
    fn stop_reasons_normalize_upstream_aliases() {
        assert_eq!(StopReason::from_upstream("MAX_OUTPUT_TOKENS"), StopReason::MaxTokens);
        assert_eq!(StopReason::from_upstream("upstream_disconnect"), StopReason::StreamIncomplete);
        assert_eq!(StopReason::from_upstream("vendor_reason").as_str(), "vendor_reason");
    }
}
