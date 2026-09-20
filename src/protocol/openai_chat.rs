use crate::protocol::internal::{
    InternalMessage, InternalRequest, InternalTool, InternalToolCall, InternalToolResult,
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

impl From<ChatRequest> for InternalRequest {
    fn from(value: ChatRequest) -> Self {
        Self {
            model: value.model,
            messages: value.messages.into_iter().map(parse_message).collect(),
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
