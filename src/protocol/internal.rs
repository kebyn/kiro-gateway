use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum InternalContent {
    Text(String),
    Thinking(String),
    ToolUse(InternalToolCall),
    ToolResult(InternalToolResult),
    Unknown(Value),
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

    /// Converts the protocol-specific JSON content into a bounded internal
    /// vocabulary. Unknown blocks are retained as opaque values so a future
    /// protocol extension cannot accidentally be interpreted as executable
    /// tool data.
    pub fn normalized_content(&self) -> Vec<InternalContent> {
        normalize_content(&self.content, 0)
            .into_iter()
            .chain(self.tool_calls.iter().cloned().map(InternalContent::ToolUse))
            .chain(self.tool_results.iter().cloned().map(InternalContent::ToolResult))
            .collect()
    }
}

const MAX_CONTENT_DEPTH: usize = 8;
const MAX_CONTENT_NODES: usize = 256;

fn normalize_content(value: &Value, depth: usize) -> Vec<InternalContent> {
    if depth > MAX_CONTENT_DEPTH {
        return vec![InternalContent::Unknown(Value::String("[content depth limit]".into()))];
    }
    match value {
        Value::String(text) => vec![InternalContent::Text(text.clone())],
        Value::Array(items) => items
            .iter()
            .take(MAX_CONTENT_NODES)
            .flat_map(|item| normalize_content(item, depth + 1))
            .collect(),
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text") | Some("output_text") | Some("input_text") => object
                .get("text")
                .or_else(|| object.get("output_text"))
                .or_else(|| object.get("input_text"))
                .and_then(Value::as_str)
                .map(|text| vec![InternalContent::Text(text.to_owned())])
                .unwrap_or_else(|| vec![InternalContent::Unknown(value.clone())]),
            Some("thinking") | Some("reasoning") => object
                .get("thinking")
                .or_else(|| object.get("text"))
                .and_then(Value::as_str)
                .map(|text| vec![InternalContent::Thinking(text.to_owned())])
                .unwrap_or_else(|| vec![InternalContent::Unknown(value.clone())]),
            _ => vec![InternalContent::Unknown(value.clone())],
        },
        _ => vec![InternalContent::Unknown(value.clone())],
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
    pub fn input_text(&self) -> String {
        self.messages.iter().map(content_text).collect::<Vec<_>>().join("\n")
    }
}

pub fn content_text(message: &InternalMessage) -> String {
    message
        .normalized_content()
        .into_iter()
        .filter_map(|content| match content {
            InternalContent::Text(text) | InternalContent::Thinking(text) => Some(text),
            InternalContent::Unknown(value) => Some(value_text(&value)),
            InternalContent::ToolUse(_) | InternalContent::ToolResult(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn value_text(value: &Value) -> String {
    let mut nodes = 0;
    value_text_bounded(value, 0, &mut nodes)
}

fn value_text_bounded(value: &Value, depth: usize, nodes: &mut usize) -> String {
    if depth > MAX_CONTENT_DEPTH || *nodes >= MAX_CONTENT_NODES {
        return String::new();
    }
    *nodes += 1;
    match value {
        Value::String(value) => value.clone(),
        Value::Array(items) => items
            .iter()
            .take(MAX_CONTENT_NODES)
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

#[cfg(test)]
mod tests {
    use super::{InternalContent, InternalMessage};
    use serde_json::json;

    #[test]
    fn normalizes_known_blocks_and_preserves_unknown_values() {
        let message = InternalMessage::new(
            "assistant",
            json!([
                {"type":"text","text":"hello"},
                {"type":"thinking","thinking":"hmm"},
                {"type":"html","text":"<b>keep</b>"}
            ]),
        );
        let content = message.normalized_content();
        assert!(matches!(content[0], InternalContent::Text(ref value) if value == "hello"));
        assert!(matches!(content[1], InternalContent::Thinking(ref value) if value == "hmm"));
        assert!(matches!(content[2], InternalContent::Unknown(_)));
    }

    #[test]
    fn does_not_walk_unbounded_json_nodes() {
        let mut value = json!("leaf");
        for _ in 0..12 {
            value = json!([value]);
        }
        let message = InternalMessage::new("user", value);
        assert!(
            message
                .normalized_content()
                .iter()
                .any(|item| matches!(item, InternalContent::Unknown(_)))
        );
    }
}
