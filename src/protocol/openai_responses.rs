use crate::protocol::internal::{
    InternalMessage, InternalRequest, InternalTool, InternalToolCall, InternalToolResult,
    content_text,
};
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
    #[serde(rename = "parallel_tool_calls")]
    pub _parallel_tool_calls: Option<bool>,
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
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("model must not be empty".into());
        }
        validate_tool_choice(self.tool_choice.as_ref())?;
        let validate_item = |item: &Value| -> Result<(), String> {
            match item.get("type").and_then(Value::as_str) {
                Some("additional_tools") => {
                    if !item.get("tools").is_some_and(Value::is_array) {
                        return Err("additional_tools requires a tools array".into());
                    }
                }
                Some("function_call") => {
                    for field in ["call_id", "name"] {
                        if item.get(field).and_then(Value::as_str).is_none() {
                            return Err(format!("function_call requires {field}"));
                        }
                    }
                }
                Some("function_call_output") => {
                    if item.get("call_id").and_then(Value::as_str).is_none() {
                        return Err("function_call_output requires call_id".into());
                    }
                }
                Some("message") => {
                    if item.get("role").and_then(Value::as_str).is_none()
                        || item.get("content").is_none()
                    {
                        return Err("message requires role and content".into());
                    }
                    let message = InternalMessage::new(
                        item.get("role").and_then(Value::as_str).unwrap_or_default(),
                        item.get("content").cloned().unwrap_or(Value::Null),
                    );
                    message.validate_content()?;
                }
                // Codex resends reasoning items as opaque history. They are
                // accepted for compatibility but are intentionally not sent
                // to the upstream model.
                Some("reasoning") => {}
                Some(other) => return Err(format!("unsupported Responses input type: {other}")),
                None => {}
            }
            Ok(())
        };
        match &self.input {
            Value::Array(items) => {
                for item in items {
                    validate_item(item)?;
                }
            }
            Value::Object(item) => validate_item(&Value::Object(item.clone()))?,
            _ => {}
        }
        for tool in &self.tools {
            if tool.get("function").is_some() || tool.get("name").and_then(Value::as_str).is_none()
            {
                return Err("Responses tools must use the flat function shape".into());
            }
        }
        Ok(())
    }

    pub fn into_internal(self, previous: Vec<InternalMessage>) -> InternalRequest {
        let mut messages = previous;
        let previous_len = messages.len();
        let mut tools = self.tools.into_iter().filter_map(parse_tool).collect::<Vec<_>>();
        let mut instructions = self.instructions;
        match self.input {
            Value::Array(items) => {
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                        tools.extend(parse_additional_tools(&item));
                        continue;
                    }
                    if is_instruction_item(&item) {
                        if let Some(content) = item.get("content").cloned() {
                            append_instruction(&mut instructions, content);
                        }
                        continue;
                    }
                    append_item(&mut messages, &item, previous_len);
                }
            }
            Value::String(text) => messages.push(InternalMessage::new("user", Value::String(text))),
            item @ Value::Object(_) => {
                if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                    tools.extend(parse_additional_tools(&item));
                } else if is_instruction_item(&item) {
                    if let Some(content) = item.get("content").cloned() {
                        append_instruction(&mut instructions, content);
                    }
                } else if !append_item(&mut messages, &item, previous_len) {
                    messages.push(InternalMessage::new("user", item));
                }
            }
            value => messages.push(InternalMessage::new("user", value)),
        }
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
            instructions,
        }
    }
}

fn validate_tool_choice(choice: Option<&Value>) -> Result<(), String> {
    let Some(choice) = choice else { return Ok(()) };
    match choice {
        Value::String(value) if value == "auto" => Ok(()),
        Value::String(value) => Err(format!("unsupported Responses tool_choice: {value}")),
        Value::Object(_) => {
            Err("named Responses tool_choice is not supported by the Kiro upstream".into())
        }
        _ => Err("Responses tool_choice must be a string or function object".into()),
    }
}

fn is_instruction_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|role| matches!(role, "system" | "developer"))
}

fn append_instruction(instructions: &mut Option<String>, content: Value) {
    let text = content_text(&InternalMessage::new("developer", content));
    if text.is_empty() {
        return;
    }
    if let Some(existing) = instructions {
        existing.push_str("\n\n");
        existing.push_str(&text);
    } else {
        *instructions = Some(text);
    }
}

fn append_item(messages: &mut Vec<InternalMessage>, item: &Value, previous_len: usize) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => true,
        Some("function_call") => {
            let Some(call) = parse_function_call(item) else {
                return false;
            };
            let can_merge = messages.len() > previous_len;
            if let Some(message) =
                messages.last_mut().filter(|message| can_merge && message.role == "assistant")
            {
                message.tool_calls.push(call);
            } else {
                let mut message = InternalMessage::new("assistant", Value::Null);
                message.tool_calls.push(call);
                messages.push(message);
            }
            true
        }
        Some("function_call_output") => {
            let Some(tool_call_id) = item.get("call_id").and_then(Value::as_str) else {
                return false;
            };
            let mut message = InternalMessage::new("tool", Value::Null);
            message.tool_call_id = Some(tool_call_id.to_owned());
            message.tool_results.push(InternalToolResult {
                tool_call_id: tool_call_id.to_owned(),
                content: item.get("output").cloned().unwrap_or(Value::Null),
                is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || item.get("status").and_then(Value::as_str) == Some("failed"),
            });
            messages.push(message);
            true
        }
        Some("message") if item.get("role").and_then(Value::as_str).is_some() => {
            let role = item.get("role").and_then(Value::as_str).unwrap();
            let Some(content) = item.get("content").cloned() else {
                return false;
            };
            let mut message = InternalMessage::new(role, content);
            message.name = item.get("name").and_then(Value::as_str).map(ToOwned::to_owned);
            messages.push(message);
            true
        }
        _ => false,
    }
}

fn parse_function_call(item: &Value) -> Option<InternalToolCall> {
    let id = item.get("call_id")?.as_str()?.to_owned();
    let name = item.get("name")?.as_str()?.to_owned();
    let (arguments, arguments_complete) =
        parse_arguments(item.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({})));
    let status_complete = !matches!(
        item.get("status").and_then(Value::as_str),
        Some("incomplete" | "failed" | "cancelled")
    );
    Some(InternalToolCall { id, name, arguments, complete: arguments_complete && status_complete })
}

fn parse_arguments(arguments: Value) -> (Value, bool) {
    match arguments {
        Value::String(arguments) if arguments.trim().is_empty() => {
            (Value::Object(Default::default()), true)
        }
        Value::String(arguments) => match serde_json::from_str::<Value>(&arguments) {
            Ok(value) if value.is_object() => (value, true),
            Ok(value) => (value, false),
            Err(_) => (Value::String(arguments), false),
        },
        Value::Object(object) => (Value::Object(object), true),
        value => (value, false),
    }
}
fn parse_tool(tool: Value) -> Option<InternalTool> {
    Some(InternalTool {
        name: kiro_tool_name(tool.get("name")?.as_str()?),
        description: tool.get("description").and_then(Value::as_str).map(ToOwned::to_owned),
        input_schema: tool
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object"})),
    })
}

fn parse_additional_tools(item: &Value) -> Vec<InternalTool> {
    item.get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|namespace| {
            let namespace_name = namespace.get("name").and_then(Value::as_str).unwrap_or_default();
            namespace.get("tools").and_then(Value::as_array).into_iter().flatten().filter_map(
                move |tool| {
                    // Kiro accepts JSON-schema function tools, not Codex custom grammars.
                    if tool.get("type").and_then(Value::as_str) == Some("custom") {
                        return None;
                    }
                    let name = tool.get("name").and_then(Value::as_str)?;
                    let qualified_name = if namespace_name.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{namespace_name}_{name}")
                    };
                    let input_schema = tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type":"object"}));
                    Some(InternalTool {
                        name: qualified_name,
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        input_schema,
                    })
                },
            )
        })
        .collect()
}

fn kiro_tool_name(name: &str) -> String {
    name.replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::ResponsesRequest;
    use crate::protocol::internal::InternalMessage;
    use serde_json::{Value, json};

    #[test]
    fn parses_parallel_calls_and_function_outputs() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking"}]},
                {"type":"function_call","id":"fc_item_a","call_id":"call_a","name":"alpha","arguments":"{\"value\":1}"},
                {"type":"function_call","id":"fc_item_b","call_id":"call_b","name":"beta","arguments":"{\"value\":2}"},
                {"type":"function_call_output","call_id":"call_a","output":"first"},
                {"type":"function_call_output","call_id":"call_b","output":{"answer":2},"status":"failed"}
            ]
        }))
        .unwrap();

        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 3);
        assert_eq!(internal.messages[0].tool_calls.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_a");
        assert_eq!(internal.messages[0].tool_calls[1].arguments["value"], 2);
        assert_eq!(internal.messages[1].tool_results[0].content, Value::String("first".into()));
        assert!(internal.messages[2].tool_results[0].is_error);
    }

    #[test]
    fn appends_function_output_to_stored_tool_call_history() {
        let mut assistant = InternalMessage::new("assistant", Value::Null);
        assistant.tool_calls.push(crate::protocol::internal::InternalToolCall {
            id: "call_saved".into(),
            name: "lookup".into(),
            arguments: json!({"id":7}),
            complete: true,
        });
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[{"type":"function_call_output","call_id":"call_saved","output":"done"}]
        }))
        .unwrap();

        let internal = request.into_internal(vec![assistant]);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_saved");
        assert_eq!(internal.messages[1].tool_results[0].tool_call_id, "call_saved");
    }

    #[test]
    fn merges_consecutive_function_calls_and_marks_invalid_arguments_incomplete() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"function_call","id":"fc_a","call_id":"call_a","name":"alpha","arguments":"{\"a\":1}"},
                {"type":"function_call","id":"fc_b","call_id":"call_b","name":"beta","arguments":"{\"b\":"},
                {"type":"message","role":"user","content":"after calls"},
                {"type":"function_call","id":"fc_c","call_id":"call_c","name":"gamma","arguments":{},"status":"incomplete"}
            ]
        }))
        .unwrap();
        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 3);
        assert_eq!(internal.messages[0].tool_calls.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_a");
        assert_eq!(internal.messages[0].tool_calls[1].id, "call_b");
        assert!(!internal.messages[0].tool_calls[1].complete);
        assert_eq!(internal.messages[1].role, "user");
        assert_eq!(internal.messages[1].content, "after calls");
        assert_eq!(internal.messages[2].tool_calls[0].id, "call_c");
        assert!(!internal.messages[2].tool_calls[0].complete);
    }

    #[test]
    fn rejects_function_call_without_call_id() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[{"type":"function_call","id":"fc_only","name":"lookup","arguments":{}}]
        }))
        .unwrap();
        let internal = request.into_internal(Vec::new());
        assert!(internal.messages.is_empty() || internal.messages[0].tool_calls.is_empty());
    }

    #[test]
    fn validation_rejects_legacy_tool_call_id_and_nested_tool_shape() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[{"type":"function_call_output","tool_call_id":"legacy","output":"x"}],
            "tools":[{"type":"function","function":{"name":"lookup"}}]
        }))
        .unwrap();
        assert!(request.validate().is_err());
    }

    #[test]
    fn accepts_codex_additional_tools_and_flattens_namespaces() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "input":[
                {
                    "type":"additional_tools",
                    "id":"at_1",
                    "role":"developer",
                    "tools":[
                        {
                            "type":"namespace",
                            "name":"functions",
                            "tools":[
                                {
                                    "type":"function",
                                    "name":"wait",
                                    "description":"Wait",
                                    "parameters":{"type":"object","properties":{"seconds":{"type":"number"}}}
                                },
                                {
                                    "type":"custom",
                                    "name":"exec",
                                    "description":"Execute code",
                                    "format":{"type":"grammar"}
                                }
                            ]
                        }
                    ]
                },
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}
            ]
        }))
        .unwrap();
        request.validate().unwrap();
        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 1);
        assert_eq!(internal.tools.len(), 1);
        assert_eq!(internal.tools[0].name, "functions_wait");
    }

    #[test]
    fn accepts_and_ignores_opaque_codex_reasoning_items() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "input":[
                {
                    "type":"reasoning",
                    "id":"rs_1",
                    "summary":[{"type":"summary_text","text":"private reasoning"}],
                    "encrypted_content":"opaque-reasoning"
                },
                {"type":"message","role":"user","content":"hello"}
            ]
        }))
        .unwrap();

        request.validate().unwrap();
        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 1);
        assert_eq!(internal.messages[0].role, "user");
        assert_eq!(internal.messages[0].content, "hello");
        assert!(!internal.input_text().contains("private reasoning"));
        assert!(!internal.input_text().contains("opaque-reasoning"));
    }

    #[test]
    fn combines_instructions_and_developer_messages() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "instructions":"Follow the policy.",
            "input":[
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"Be concise."}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}
            ]
        }))
        .unwrap();

        request.validate().unwrap();
        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 1);
        assert_eq!(internal.messages[0].role, "user");
        assert_eq!(internal.instructions.as_deref(), Some("Follow the policy.\n\nBe concise."));
    }

    #[test]
    fn rejects_non_text_message_content() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:..."}]}
            ]
        }))
        .unwrap();

        assert!(request.validate().unwrap_err().contains("unsupported content"));
    }

    #[test]
    fn rejects_forced_tool_choice_instead_of_ignoring_it() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "tool_choice":"required",
            "input":"hello"
        }))
        .unwrap();
        assert!(request.validate().unwrap_err().contains("tool_choice"));
    }

    #[test]
    fn reasoning_input_object_does_not_fall_back_to_a_user_message() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "input":{
                "type":"reasoning",
                "id":"rs_1",
                "encrypted_content":"opaque-reasoning"
            }
        }))
        .unwrap();

        request.validate().unwrap();
        let internal = request.into_internal(Vec::new());
        assert!(internal.messages.is_empty());
    }

    #[test]
    fn reasoning_items_do_not_break_tool_call_order() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "input":[
                {"type":"reasoning","id":"rs_1","summary":[]},
                {"type":"function_call","call_id":"call_a","name":"lookup","arguments":"{}"},
                {"type":"reasoning","id":"rs_2","summary":[]},
                {"type":"function_call_output","call_id":"call_a","output":"done"}
            ]
        }))
        .unwrap();

        request.validate().unwrap();
        let internal = request.into_internal(Vec::new());
        assert_eq!(internal.messages.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_a");
        assert_eq!(internal.messages[1].tool_results[0].tool_call_id, "call_a");
    }

    #[test]
    fn does_not_merge_first_new_call_into_previous_response_assistant() {
        let mut previous = InternalMessage::new("assistant", Value::Null);
        previous.tool_calls.push(crate::protocol::internal::InternalToolCall {
            id: "call_previous".into(),
            name: "old".into(),
            arguments: json!({}),
            complete: true,
        });
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"function_call","call_id":"call_new_a","name":"alpha","arguments":{}},
                {"type":"function_call","call_id":"call_new_b","name":"beta","arguments":{}}
            ]
        }))
        .unwrap();
        let internal = request.into_internal(vec![previous]);
        assert_eq!(internal.messages.len(), 2);
        assert_eq!(internal.messages[0].tool_calls[0].id, "call_previous");
        assert_eq!(internal.messages[1].tool_calls.len(), 2);
        assert_eq!(internal.messages[1].tool_calls[0].id, "call_new_a");
        assert_eq!(internal.messages[1].tool_calls[1].id, "call_new_b");
    }
}
