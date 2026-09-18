pub mod cli;
pub mod ide;

use crate::{
    auth::{AuthMethod, Credential},
    error::AppError,
    protocol::internal::{
        InternalMessage, InternalRequest, InternalToolResult, content_text, value_text,
    },
    transform::tool_compression::compress_schema,
};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    Ide,
    Cli,
}

impl EndpointKind {
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("cli") { Self::Cli } else { Self::Ide }
    }
}

pub trait KiroEndpoint: Send + Sync {
    fn kind(&self) -> EndpointKind;
    fn api_url(&self, credential: &Credential) -> String;
    fn mcp_url(&self, credential: &Credential) -> String;
    fn transform_api_body(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> serde_json::Value;
    fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder;
    fn decorate_mcp(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder;
    fn classify_error(&self, status: reqwest::StatusCode, body: &str) -> AppError;
}

pub fn endpoint_for(kind: EndpointKind, upstream_url: Option<&str>) -> Box<dyn KiroEndpoint> {
    match kind {
        EndpointKind::Cli => Box::new(cli::CliEndpoint::new(upstream_url.map(str::to_owned))),
        EndpointKind::Ide => Box::new(ide::IdeEndpoint::new(upstream_url.map(str::to_owned))),
    }
}

pub fn conversation_body(
    request: &InternalRequest,
    credential: &Credential,
    origin: &str,
    model_id: &str,
) -> serde_json::Value {
    // Kiro rejects native tool result structures when no tool definitions are
    // present. In that case all tool history is intentionally rendered as text.
    let active_tool_round =
        (!request.tools.is_empty()).then(|| active_tool_round(&request.messages)).flatten();
    let current_start = active_tool_round
        .map(|index| index + 1)
        .unwrap_or_else(|| request.messages.len().saturating_sub(1));
    let mut history: Vec<serde_json::Value> = request.messages[..current_start]
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if active_tool_round == Some(index) {
                assistant_tool_message(message)
            } else {
                message_to_history(message, origin, model_id)
            }
        })
        .collect();
    if request.messages.is_empty() {
        history.clear();
    }
    let tools: Vec<serde_json::Value> = request.tools.iter().map(|tool| serde_json::json!({"toolSpecification":{"inputSchema":{"json":compress_schema(&tool.input_schema, 32 * 1024)},"name":tool.name,"description":tool.description.clone().unwrap_or_default()}})).collect();
    let current_messages = request.messages.get(current_start..).unwrap_or_default();
    let current_content = current_messages
        .iter()
        .map(|message| {
            if active_tool_round.is_some() { content_text(message) } else { history_text(message) }
        })
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let current_content =
        if current_content.is_empty() { "(empty placeholder)".to_owned() } else { current_content };
    let active_ids: HashSet<&str> = active_tool_round
        .and_then(|index| request.messages.get(index))
        .into_iter()
        .flat_map(|message| message.tool_calls.iter().map(|call| call.id.as_str()))
        .collect();
    let mut seen_results = HashSet::new();
    let tool_results: Vec<serde_json::Value> = current_messages
        .iter()
        .flat_map(|message| &message.tool_results)
        .filter(|result| active_ids.contains(result.tool_call_id.as_str()))
        .filter(|result| seen_results.insert(result.tool_call_id.clone()))
        .map(tool_result)
        .collect();
    let mut context = serde_json::Map::new();
    if !tools.is_empty() {
        context.insert("tools".into(), serde_json::Value::Array(tools));
    }
    if !tool_results.is_empty() {
        context.insert("toolResults".into(), serde_json::Value::Array(tool_results));
    }
    let mut user_input = serde_json::json!({
        "content":current_content,
        "modelId":model_id,
        "origin":origin,
    });
    if !context.is_empty() {
        user_input["userInputMessageContext"] = serde_json::Value::Object(context);
    }
    let current = serde_json::json!({"userInputMessage":user_input});
    let conversation_id =
        request.conversation_id.clone().unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let state = serde_json::json!({"conversationId":conversation_id,"history":history,"currentMessage":current,"chatTriggerType":"MANUAL","agentTaskType":"vibe"});
    let mut body = serde_json::json!({"conversationState":state});
    if !matches!(credential.auth_method, AuthMethod::Sso) {
        if let Some(profile_arn) = &credential.profile_arn {
            body["profileArn"] = serde_json::Value::String(profile_arn.clone());
        }
    }
    body
}

fn active_tool_round(messages: &[InternalMessage]) -> Option<usize> {
    if messages.last().is_none_or(|message| message.tool_results.is_empty()) {
        return None;
    }
    messages.iter().enumerate().rev().find_map(|(index, message)| {
        if message.role != "assistant" || message.tool_calls.is_empty() {
            return None;
        }
        let call_ids: HashSet<&str> =
            message.tool_calls.iter().map(|call| call.id.as_str()).collect();
        let following = &messages[index + 1..];
        let mut result_ids = HashSet::new();
        let valid = !following.is_empty()
            && following.iter().all(|candidate| {
                !candidate.tool_results.is_empty()
                    && candidate.tool_results.iter().all(|result| {
                        let known = call_ids.contains(result.tool_call_id.as_str());
                        if known {
                            result_ids.insert(result.tool_call_id.as_str());
                        }
                        known
                    })
            })
            && result_ids.len() == call_ids.len();
        valid.then_some(index)
    })
}

fn assistant_tool_message(message: &InternalMessage) -> serde_json::Value {
    let content = content_text(message);
    let content = if content.is_empty() { "(empty placeholder)".to_owned() } else { content };
    let tool_uses: Vec<serde_json::Value> = message
        .tool_calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "toolUseId": call.id,
                "name": call.name,
                "input": call.arguments,
            })
        })
        .collect();
    serde_json::json!({
        "assistantResponseMessage": {
            "content": content,
            "toolUses": tool_uses,
        }
    })
}

fn tool_result(result: &InternalToolResult) -> serde_json::Value {
    serde_json::json!({
        "toolUseId": result.tool_call_id,
        "content": tool_result_content(&result.content),
        "status": if result.is_error { "error" } else { "success" },
    })
}

fn tool_result_content(value: &serde_json::Value) -> Vec<serde_json::Value> {
    match value {
        serde_json::Value::Array(items) if items.is_empty() => {
            vec![serde_json::json!({"text": "(empty result)"})]
        }
        serde_json::Value::Array(items) => items.iter().flat_map(tool_result_content).collect(),
        serde_json::Value::String(text) => {
            vec![serde_json::json!({"text": if text.is_empty() { "(empty result)" } else { text }})]
        }
        serde_json::Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
                vec![
                    serde_json::json!({"text": if text.is_empty() { "(empty result)" } else { text }}),
                ]
            } else {
                vec![serde_json::json!({"text": value.to_string()})]
            }
        }
        serde_json::Value::Null => vec![serde_json::json!({"text": "(empty result)"})],
        _ => vec![serde_json::json!({"text": value.to_string()})],
    }
}

fn message_to_history(
    message: &InternalMessage,
    origin: &str,
    model_id: &str,
) -> serde_json::Value {
    match message.role.as_str() {
        "assistant" => {
            serde_json::json!({"assistantResponseMessage":{"content":history_text(message)}})
        }
        _ => {
            serde_json::json!({"userInputMessage":{"content":history_text(message),"modelId":model_id,"origin":origin,"userInputMessageContext":{}}})
        }
    }
}

fn history_text(message: &InternalMessage) -> String {
    let mut parts = Vec::new();
    let content = content_text(message);
    if !content.is_empty() {
        parts.push(content);
    }
    parts.extend(
        message.tool_calls.iter().map(|call| {
            format!("[Tool call {} ({})]\n{}", call.name, call.id, call.arguments_json())
        }),
    );
    parts.extend(message.tool_results.iter().map(|result| {
        let status = if result.is_error { " error" } else { "" };
        let content = value_text(&result.content);
        let content = if content.is_empty() { result.content.to_string() } else { content };
        format!("[Tool result {}{}]\n{}", result.tool_call_id, status, content)
    }));
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::conversation_body;
    use crate::{
        auth::Credential,
        protocol::internal::{
            InternalMessage, InternalRequest, InternalToolCall, InternalToolResult,
        },
    };
    use serde_json::{Value, json};

    fn request(messages: Vec<InternalMessage>) -> InternalRequest {
        InternalRequest {
            model: "kiro".into(),
            messages,
            system: None,
            tools: vec![
                crate::protocol::internal::InternalTool {
                    name: "alpha".into(),
                    description: None,
                    input_schema: json!({"type":"object"}),
                },
                crate::protocol::internal::InternalTool {
                    name: "beta".into(),
                    description: None,
                    input_schema: json!({"type":"object"}),
                },
            ],
            tool_choice: None,
            stream: false,
            max_tokens: None,
            temperature: None,
            conversation_id: Some("conversation".into()),
            instructions: None,
        }
    }

    fn call(id: &str, name: &str) -> InternalToolCall {
        InternalToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({"id":id}),
            complete: true,
        }
    }

    fn result(id: &str, content: Value, is_error: bool) -> InternalMessage {
        let mut message = InternalMessage::new("tool", Value::Null);
        message.tool_results.push(InternalToolResult {
            tool_call_id: id.into(),
            content,
            is_error,
        });
        message
    }

    #[test]
    fn sends_only_active_tool_round_as_native_kiro_payload() {
        let mut old_assistant = InternalMessage::new("assistant", Value::Null);
        old_assistant.tool_calls.push(call("old", "old_lookup"));
        let mut active_assistant =
            InternalMessage::new("assistant", Value::String("Checking".into()));
        active_assistant.tool_calls.push(call("call_a", "alpha"));
        active_assistant.tool_calls.push(call("call_b", "beta"));
        let body = conversation_body(
            &request(vec![
                InternalMessage::new("user", Value::String("old question".into())),
                old_assistant,
                result("old", Value::String("old result".into()), false),
                InternalMessage::new("assistant", Value::String("old answer".into())),
                InternalMessage::new("user", Value::String("new question".into())),
                active_assistant,
                result("call_a", Value::String("first".into()), false),
                result("call_b", json!({"answer":2}), true),
            ]),
            &Credential::default(),
            "AI_EDITOR",
            "kiro",
        );

        let state = &body["conversationState"];
        let history = state["history"].as_array().unwrap();
        assert!(history[1]["assistantResponseMessage"]["toolUses"].is_null());
        assert!(
            history[1]["assistantResponseMessage"]["content"]
                .as_str()
                .unwrap()
                .contains("old_lookup")
        );
        let active =
            history.last().unwrap()["assistantResponseMessage"]["toolUses"].as_array().unwrap();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0]["toolUseId"], "call_a");
        let results =
            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"]
                .as_array()
                .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["toolUseId"], "call_a");
        assert_eq!(results[0]["content"][0]["text"], "first");
        assert_eq!(results[1]["toolUseId"], "call_b");
        assert_eq!(results[1]["status"], "error");
    }

    #[test]
    fn renders_tool_history_as_text_when_no_tools_are_declared() {
        let mut assistant = InternalMessage::new("assistant", Value::Null);
        assistant.tool_calls.push(call("call_a", "alpha"));
        let body = conversation_body(
            &InternalRequest {
                tools: Vec::new(),
                ..request(vec![
                    InternalMessage::new("user", Value::String("question".into())),
                    assistant,
                    result("call_a", Value::String("done".into()), false),
                ])
            },
            &Credential::default(),
            "AI_EDITOR",
            "kiro",
        );
        let state = &body["conversationState"];
        assert!(
            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                .get("toolResults")
                .is_none()
        );
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["content"],
            r#"[Tool call alpha (call_a)]
{"id":"call_a"}"#
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["content"],
            "[Tool result call_a]\ndone"
        );
    }
}
