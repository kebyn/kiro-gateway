use crate::protocol::internal::{
    InternalMessage, InternalRequest, InternalTool, InternalToolCall, InternalToolResult,
    content_text,
};
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
    pub tool_calls: Vec<ChatToolCall>,
}
#[derive(Debug, Deserialize)]
pub struct ChatToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ChatFunctionCall,
}
#[derive(Debug, Deserialize)]
pub struct ChatFunctionCall {
    pub name: String,
    pub arguments: String,
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

impl ChatRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_tool_choice(self.tool_choice.as_ref())?;
        for message in &self.messages {
            let content = message.content.clone().unwrap_or(Value::Null);
            InternalMessage::new(message.role.as_str(), content).validate_content()?;
            for call in &message.tool_calls {
                if call.r#type != "function" {
                    return Err(format!("unsupported Chat tool call type: {}", call.r#type));
                }
            }
        }
        for tool in &self.tools {
            if tool.r#type != "function" {
                return Err(format!("unsupported Chat tool type: {}", tool.r#type));
            }
        }
        Ok(())
    }
}

fn validate_tool_choice(choice: Option<&Value>) -> Result<(), String> {
    let Some(choice) = choice else { return Ok(()) };
    match choice {
        Value::String(value) if value == "auto" => Ok(()),
        Value::String(value) => Err(format!("unsupported Chat tool_choice: {value}")),
        Value::Object(_) => {
            Err("named Chat tool_choice is not supported by the Kiro upstream".into())
        }
        _ => Err("Chat tool_choice must be a string or function object".into()),
    }
}

impl From<ChatRequest> for InternalRequest {
    fn from(value: ChatRequest) -> Self {
        let mut system_parts = Vec::new();
        let mut messages = Vec::new();
        for message in value.messages {
            let parsed = parse_message(message);
            if matches!(parsed.role.as_str(), "system" | "developer") {
                let text = content_text(&parsed);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            } else {
                messages.push(parsed);
            }
        }
        Self {
            model: value.model,
            messages,
            system: (!system_parts.is_empty()).then(|| system_parts.join("\n\n")),
            tools: value
                .tools
                .into_iter()
                .filter(|t| t.r#type == "function")
                .map(|t| InternalTool {
                    name: t.function.name,
                    description: t.function.description,
                    input_schema: t.function.parameters,
                    custom: false,
                    original_name: None,
                    namespace: None,
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

fn parse_message(message: ChatMessage) -> InternalMessage {
    let content = message.content.unwrap_or(Value::Null);
    let mut parsed = InternalMessage::new(message.role, content.clone());
    parsed.name = message.name;
    parsed.tool_call_id = message.tool_call_id.clone();
    parsed.tool_calls = message
        .tool_calls
        .into_iter()
        .filter(|call| call.r#type == "function")
        .map(|call| {
            let (arguments, complete) = parse_arguments(Value::String(call.function.arguments));
            InternalToolCall { id: call.id, name: call.function.name, arguments, complete }
        })
        .collect();
    if parsed.role == "tool" {
        if let Some(tool_call_id) = message.tool_call_id {
            parsed.tool_results.push(InternalToolResult { tool_call_id, content, is_error: false });
            parsed.content = Value::Null;
        }
    }
    parsed
}

fn parse_arguments(arguments: Value) -> (Value, bool) {
    match arguments {
        Value::String(arguments) if arguments.trim().is_empty() => (serde_json::json!({}), true),
        Value::String(arguments) => match serde_json::from_str::<Value>(&arguments) {
            Ok(value) if value.is_object() => (value, true),
            Ok(value) => (value, false),
            Err(_) => (Value::String(arguments), false),
        },
        Value::Object(object) => (Value::Object(object), true),
        value => (value, false),
    }
}

#[cfg(test)]
mod tests {
    use super::ChatRequest;
    use crate::protocol::internal::InternalRequest;

    #[test]
    fn preserves_parallel_calls_and_tool_messages() {
        let request: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[
                {"role":"assistant","content":"Working","tool_calls":[
                    {"id":"call_a","type":"function","function":{"name":"alpha","arguments":"{\"value\":1}"}},
                    {"id":"call_b","type":"function","function":{"name":"beta","arguments":"{\"value\":2}"}}
                ]},
                {"role":"tool","tool_call_id":"call_a","content":"first"},
                {"role":"tool","tool_call_id":"call_b","content":[{"type":"text","text":"second"}]}
            ]
        }))
        .unwrap();

        let internal: InternalRequest = request.into();
        assert_eq!(internal.messages[0].tool_calls.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[1].arguments["value"], 2);
        assert_eq!(internal.messages[1].tool_results[0].tool_call_id, "call_a");
        assert_eq!(internal.messages[2].tool_results[0].content[0]["text"], "second");
    }

    #[test]
    fn maps_system_and_developer_roles_to_instructions() {
        let request: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[
                {"role":"system","content":"Follow the policy."},
                {"role":"developer","content":[{"type":"text","text":"Be concise."}]},
                {"role":"user","content":"hello"}
            ]
        }))
        .unwrap();

        let internal: InternalRequest = request.into();
        assert_eq!(internal.messages.len(), 1);
        assert_eq!(internal.messages[0].role, "user");
        assert_eq!(internal.system.as_deref(), Some("Follow the policy.\n\nBe concise."));
    }

    #[test]
    fn rejects_unsupported_tool_and_content_types() {
        let request: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "tools":[{"type":"web_search","function":{"name":"search","parameters":{}}}],
            "messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.test/image"}}]}]
        }))
        .unwrap();
        let error = request.validate().unwrap_err();
        assert!(error.contains("unsupported content"));
    }

    #[test]
    fn rejects_named_tool_choice_instead_of_ignoring_it() {
        let request: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "tool_choice":{"type":"function","function":{"name":"lookup"}},
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert!(request.validate().unwrap_err().contains("tool_choice"));
    }

    #[test]
    fn marks_truncated_function_arguments_incomplete() {
        let request: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"assistant","tool_calls":[
                {"id":"call_partial","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}
            ]}]
        }))
        .unwrap();
        let internal: InternalRequest = request.into();
        assert!(!internal.messages[0].tool_calls[0].complete);
        assert_eq!(internal.messages[0].tool_calls[0].arguments, "{\"q\":");
    }
}
