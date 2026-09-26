use crate::generation::{
    ContentPart, GenerationRequest, Message, Role, ToolCall, ToolDefinition, ToolResult,
    content_text,
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
            parse_text_parts(system)?;
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
            parse_text_parts(system)?;
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
        .map(|_| ())
}

impl MessagesRequest {
    pub fn into_generation(self) -> Result<GenerationRequest, String> {
        Ok(GenerationRequest {
            model: self.model,
            messages: self
                .messages
                .into_iter()
                .map(parse_message)
                .collect::<Result<Vec<_>, _>>()?,
            system: self.system.map(|value| text_value(&value)),
            tools: self
                .tools
                .into_iter()
                .map(|tool| ToolDefinition {
                    name: tool.name,
                    description: tool.description,
                    input_schema: tool.input_schema,
                    custom: false,
                    original_name: None,
                    namespace: None,
                })
                .collect(),
            stream: self.stream,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            conversation_id: None,
            instructions: None,
            opaque_history: Vec::new(),
        })
    }
}

pub(crate) fn parse_message(message: AnthropicMessage) -> Result<Message, String> {
    let role = Role::parse(&message.role)?;
    if !matches!(role, Role::User | Role::Assistant) {
        return Err(format!("unsupported Anthropic message role: {role}"));
    }
    let mut parsed = Message::empty(role);
    match message.content {
        Value::Array(items) => {
            for item in items {
                parse_content_block(item, &mut parsed)?;
            }
        }
        value => parsed.content = parse_text_parts(&value)?,
    }
    Ok(parsed)
}

fn parse_content_block(item: Value, message: &mut Message) -> Result<(), String> {
    match item.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic tool_use requires id".to_owned())?;
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic tool_use requires name".to_owned())?;
            let input = item.get("input").cloned().unwrap_or_else(|| serde_json::json!({}));
            message.tool_calls.push(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                complete: input.is_object(),
                arguments: input,
            });
        }
        Some("tool_result") => {
            let tool_call_id = item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic tool_result requires tool_use_id".to_owned())?;
            message.tool_results.push(ToolResult {
                tool_call_id: tool_call_id.to_owned(),
                content: item.get("content").cloned().unwrap_or(Value::Null),
                is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            });
        }
        _ => message.content.extend(parse_text_parts(&item)?),
    }
    Ok(())
}

fn parse_text_parts(value: &Value) -> Result<Vec<ContentPart>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![ContentPart::text(text)]),
        Value::Array(items) => items.iter().try_fold(Vec::new(), |mut parts, item| {
            parts.extend(parse_text_parts(item)?);
            Ok(parts)
        }),
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text" | "input_text" | "output_text") => object
                .get("text")
                .or_else(|| object.get("input_text"))
                .or_else(|| object.get("output_text"))
                .and_then(Value::as_str)
                .map(|text| vec![ContentPart::text(text)])
                .ok_or_else(|| "Anthropic text block requires text".to_owned()),
            Some("thinking" | "reasoning") => object
                .get("thinking")
                .or_else(|| object.get("text"))
                .and_then(Value::as_str)
                .map(|text| vec![ContentPart::thinking(text)])
                .ok_or_else(|| "Anthropic thinking block requires text".to_owned()),
            _ => Err("unsupported content in Anthropic message; only text, thinking, and tool blocks are supported".into()),
        },
        _ => Err("unsupported content in Anthropic message; only text content is supported".into()),
    }
}

pub(crate) fn text_value(value: &Value) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| parse_text_parts(item).ok())
            .map(|content| content_text(&Message::new(Role::System, content)))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => parse_text_parts(value)
            .map(|content| content_text(&Message::new(Role::System, content)))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::MessagesRequest;
    use crate::generation::GenerationRequest;

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

        let internal: GenerationRequest = request.into_generation().unwrap();
        assert_eq!(internal.messages[0].tool_calls.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].arguments["city"], "Paris");
        assert_eq!(internal.messages[1].tool_results.len(), 2);
        assert_eq!(internal.messages[1].tool_results[0].tool_call_id, "call_weather");
        assert!(internal.messages[1].tool_results[1].is_error);
        assert_eq!(crate::generation::content_text(&internal.messages[0]), "Checking");
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

        let internal: GenerationRequest = request.into_generation().unwrap();
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
        let internal: GenerationRequest = request.into_generation().unwrap();
        assert!(!internal.messages[0].tool_calls[0].complete);
        assert_eq!(internal.messages[0].tool_calls[0].arguments, "{\"q\":");
    }
}
