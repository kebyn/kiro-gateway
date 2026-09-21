use crate::protocol::internal::{
    InternalMessage, InternalRequest, InternalTool, InternalToolCall, InternalToolResult,
};
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
    #[serde(rename = "model")]
    pub _model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    #[serde(rename = "system")]
    pub _system: Option<Value>,
    #[serde(default)]
    pub tools: Vec<AnthropicTool>,
}
#[derive(Debug, Serialize)]
pub struct CountTokensResponse {
    pub input_tokens: u64,
}

impl MessagesRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_tool_choice(self.tool_choice.as_ref())?;
        if let Some(system) = &self.system {
            InternalMessage::new("system", system.clone()).validate_content()?;
        }
        for message in &self.messages {
            validate_message_content(message)?;
        }
        Ok(())
    }
}

impl CountTokensRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(system) = &self._system {
            InternalMessage::new("system", system.clone()).validate_content()?;
        }
        for message in &self.messages {
            validate_message_content(message)?;
        }
        Ok(())
    }
}

fn validate_tool_choice(choice: Option<&Value>) -> Result<(), String> {
    let Some(choice) = choice else { return Ok(()) };
    let kind = choice.get("type").and_then(Value::as_str).unwrap_or_default();
    if kind == "auto" {
        Ok(())
    } else {
        Err("Anthropic tool_choice values other than auto are not supported by the Kiro upstream"
            .into())
    }
}

fn validate_message_content(message: &AnthropicMessage) -> Result<(), String> {
    parse_message(AnthropicMessage { role: message.role.clone(), content: message.content.clone() })
        .validate_content()
}

impl From<MessagesRequest> for InternalRequest {
    fn from(value: MessagesRequest) -> Self {
        Self {
            model: value.model,
            messages: value.messages.into_iter().map(parse_message).collect(),
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

pub(crate) fn parse_message(message: AnthropicMessage) -> InternalMessage {
    let mut parsed = InternalMessage::new(message.role, Value::Null);
    let mut content = Vec::new();
    match message.content {
        Value::Array(items) => {
            for item in items {
                parse_content_block(item, &mut content, &mut parsed);
            }
            parsed.content = Value::Array(content);
        }
        value => parsed.content = value,
    }
    parsed
}

fn parse_content_block(item: Value, content: &mut Vec<Value>, message: &mut InternalMessage) {
    match item.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            if let (Some(id), Some(name)) =
                (item.get("id").and_then(Value::as_str), item.get("name").and_then(Value::as_str))
            {
                let input = item.get("input").cloned().unwrap_or_else(|| serde_json::json!({}));
                message.tool_calls.push(InternalToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    complete: input.is_object(),
                    arguments: input,
                });
            }
        }
        Some("tool_result") => {
            if let Some(tool_call_id) = item.get("tool_use_id").and_then(Value::as_str) {
                message.tool_results.push(InternalToolResult {
                    tool_call_id: tool_call_id.to_owned(),
                    content: item.get("content").cloned().unwrap_or(Value::Null),
                    is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
        _ => content.push(item),
    }
}

pub(crate) fn text_value(value: &Value) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                crate::protocol::internal::content_text(&InternalMessage::new(
                    "system",
                    item.clone(),
                ))
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => {
            crate::protocol::internal::content_text(&InternalMessage::new("system", value.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MessagesRequest;
    use crate::protocol::internal::InternalRequest;

    #[test]
    fn parses_mixed_tool_calls_and_results() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "kiro",
            "messages": [
                {"role":"assistant","content":[
                    {"type":"text","text":"Checking"},
                    {"type":"tool_use","id":"call_weather","name":"weather","input":{"city":"Paris"}},
                    {"type":"tool_use","id":"call_time","name":"time","input":{"zone":"UTC"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_weather","content":"sunny"},
                    {"type":"tool_result","tool_use_id":"call_time","content":[{"type":"text","text":"12:00"}],"is_error":true}
                ]}
            ]
        }))
        .unwrap();

        let internal: InternalRequest = request.into();
        assert_eq!(internal.messages[0].tool_calls.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].arguments["city"], "Paris");
        assert_eq!(internal.messages[1].tool_results.len(), 2);
        assert_eq!(internal.messages[1].tool_results[0].tool_call_id, "call_weather");
        assert!(internal.messages[1].tool_results[1].is_error);
        assert_eq!(crate::protocol::internal::content_text(&internal.messages[0]), "Checking");
    }

    #[test]
    fn preserves_system_instruction_text() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "system":[
                {"type":"text","text":"Follow the policy."},
                {"type":"text","text":"Be concise."}
            ],
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();

        let internal: InternalRequest = request.into();
        assert_eq!(internal.system.as_deref(), Some("Follow the policy.\nBe concise."));
    }

    #[test]
    fn rejects_non_text_system_and_message_blocks() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "system":[{"type":"image","source":{"type":"url","url":"https://example.test/image"}}],
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert!(request.validate().unwrap_err().contains("unsupported content"));
    }

    #[test]
    fn rejects_forced_tool_choice_instead_of_ignoring_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "tool_choice":{"type":"any"},
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert!(request.validate().unwrap_err().contains("tool_choice"));
    }

    #[test]
    fn marks_non_object_tool_input_incomplete() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"assistant","content":[
                {"type":"tool_use","id":"call_partial","name":"lookup","input":"{\"q\":"}
            ]}]
        }))
        .unwrap();
        let internal: InternalRequest = request.into();
        assert!(!internal.messages[0].tool_calls[0].complete);
        assert_eq!(internal.messages[0].tool_calls[0].arguments, "{\"q\":");
    }
}
