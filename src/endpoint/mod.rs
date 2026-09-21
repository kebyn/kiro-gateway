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

struct ActiveToolRound {
    index: usize,
    call_ids: HashSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    Ide,
    Cli,
}

impl EndpointKind {
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "ide" => Ok(Self::Ide),
            "cli" => Ok(Self::Cli),
            _ => Err(AppError::Config(format!("unsupported endpoint: {value}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointPolicy {
    Auto,
    Ide,
    Cli,
}

impl EndpointPolicy {
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "auto" => Ok(Self::Auto),
            "cli" => Ok(Self::Cli),
            "ide" => Ok(Self::Ide),
            _ => Err(AppError::Config(format!("unsupported endpoint: {value}"))),
        }
    }

    pub fn resolve(self, credential_endpoint: &str) -> Result<EndpointKind, AppError> {
        match self {
            Self::Auto => EndpointKind::parse(credential_endpoint),
            Self::Ide => Ok(EndpointKind::Ide),
            Self::Cli => Ok(EndpointKind::Cli),
        }
    }
}

pub trait KiroEndpoint: Send + Sync {
    fn api_url(&self, credential: &Credential) -> String;
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
    fn classify_error(&self, status: reqwest::StatusCode, body: &str) -> AppError;
}

#[derive(Clone, Debug)]
pub enum EndpointAdapter {
    Ide(ide::IdeEndpoint),
    Cli(cli::CliEndpoint),
}

impl EndpointAdapter {
    pub fn api_url(&self, credential: &Credential) -> String {
        match self {
            Self::Ide(endpoint) => endpoint.api_url(credential),
            Self::Cli(endpoint) => endpoint.api_url(credential),
        }
    }

    pub fn transform_api_body(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> serde_json::Value {
        match self {
            Self::Ide(endpoint) => endpoint.transform_api_body(request, credential),
            Self::Cli(endpoint) => endpoint.transform_api_body(request, credential),
        }
    }

    pub fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::Ide(endpoint) => endpoint.decorate_api(builder, credential),
            Self::Cli(endpoint) => endpoint.decorate_api(builder, credential),
        }
    }

    pub fn classify_error(&self, status: reqwest::StatusCode, body: &str) -> AppError {
        match self {
            Self::Ide(endpoint) => endpoint.classify_error(status, body),
            Self::Cli(endpoint) => endpoint.classify_error(status, body),
        }
    }
}

pub fn endpoint_for(kind: EndpointKind, upstream_url: Option<&str>) -> EndpointAdapter {
    match kind {
        EndpointKind::Cli => {
            EndpointAdapter::Cli(cli::CliEndpoint::new(upstream_url.map(str::to_owned)))
        }
        EndpointKind::Ide => {
            EndpointAdapter::Ide(ide::IdeEndpoint::new(upstream_url.map(str::to_owned)))
        }
    }
}

#[cfg(test)]
mod endpoint_policy_tests {
    use super::{EndpointKind, EndpointPolicy};

    #[test]
    fn auto_policy_uses_credential_endpoint() {
        assert_eq!(EndpointPolicy::Auto.resolve("cli").unwrap(), EndpointKind::Cli);
        assert_eq!(EndpointPolicy::Auto.resolve("ide").unwrap(), EndpointKind::Ide);
        assert!(EndpointPolicy::Auto.resolve("unknown").is_err());
    }

    #[test]
    fn explicit_policy_overrides_credential_endpoint() {
        assert_eq!(EndpointPolicy::Ide.resolve("cli").unwrap(), EndpointKind::Ide);
        assert_eq!(EndpointPolicy::Cli.resolve("ide").unwrap(), EndpointKind::Cli);
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
    let declared_tools: HashSet<&str> =
        request.tools.iter().map(|tool| tool.name.as_str()).collect();
    let active_tool_round = (!declared_tools.is_empty())
        .then(|| active_tool_round(&request.messages, &declared_tools))
        .flatten();
    let current_start = active_tool_round
        .as_ref()
        .map(|round| round.index + 1)
        .unwrap_or_else(|| request.messages.len().saturating_sub(1));
    let mut history: Vec<serde_json::Value> = request.messages[..current_start]
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if active_tool_round.as_ref().is_some_and(|round| round.index == index) {
                assistant_tool_message(message, &active_tool_round.as_ref().unwrap().call_ids)
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
            active_tool_round.as_ref().map_or_else(
                || history_text(message),
                |round| active_round_text(message, &round.call_ids),
            )
        })
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let current_content =
        if current_content.is_empty() { "(empty placeholder)".to_owned() } else { current_content };
    let current_content = with_instruction_context(
        request.system.as_deref(),
        request.instructions.as_deref(),
        current_content,
    );
    let mut seen_results = HashSet::new();
    let tool_results: Vec<serde_json::Value> = current_messages
        .iter()
        .flat_map(|message| &message.tool_results)
        .filter(|result| {
            active_tool_round
                .as_ref()
                .is_some_and(|round| round.call_ids.contains(result.tool_call_id.as_str()))
        })
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
    if !matches!(credential.auth_method, AuthMethod::Oidc) {
        if let Some(profile_arn) = &credential.profile_arn {
            body["profileArn"] = serde_json::Value::String(profile_arn.clone());
        }
    }
    body
}

fn with_instruction_context(
    system: Option<&str>,
    instructions: Option<&str>,
    current_content: String,
) -> String {
    let mut sections = Vec::new();
    if let Some(system) = system.filter(|value| !value.is_empty()) {
        sections.push(format!("[System instructions]\n{system}"));
    }
    if let Some(instructions) = instructions.filter(|value| !value.is_empty()) {
        sections.push(format!("[Developer instructions]\n{instructions}"));
    }
    if sections.is_empty() {
        current_content
    } else {
        sections.push(current_content);
        sections.join("\n\n")
    }
}

fn active_tool_round(
    messages: &[InternalMessage],
    declared_tools: &HashSet<&str>,
) -> Option<ActiveToolRound> {
    if messages.last().is_none_or(|message| message.tool_results.is_empty()) {
        return None;
    }
    messages.iter().enumerate().rev().find_map(|(index, message)| {
        if message.role != "assistant" || message.tool_calls.is_empty() {
            return None;
        }
        let following = &messages[index + 1..];
        if following.is_empty()
            || !following.iter().all(|candidate| {
                candidate.tool_calls.is_empty() && !candidate.tool_results.is_empty()
            })
        {
            return None;
        }
        let call_ids: HashSet<String> = message
            .tool_calls
            .iter()
            .filter(|call| call.complete && declared_tools.contains(call.name.as_str()))
            .map(|call| call.id.clone())
            .collect();
        let result_ids: HashSet<&str> = following
            .iter()
            .flat_map(|candidate| &candidate.tool_results)
            .map(|result| result.tool_call_id.as_str())
            .collect();
        (!call_ids.is_empty() && call_ids.iter().all(|id| result_ids.contains(id.as_str())))
            .then_some(ActiveToolRound { index, call_ids })
    })
}

fn assistant_tool_message(
    message: &InternalMessage,
    structured_ids: &HashSet<String>,
) -> serde_json::Value {
    let mut content = Vec::new();
    let message_content = content_text(message);
    if !message_content.is_empty() {
        content.push(message_content);
    }
    content.extend(
        message
            .tool_calls
            .iter()
            .filter(|call| !structured_ids.contains(call.id.as_str()))
            .map(tool_call_text),
    );
    let content =
        if content.is_empty() { "(empty placeholder)".to_owned() } else { content.join("\n") };
    let tool_uses: Vec<serde_json::Value> = message
        .tool_calls
        .iter()
        .filter(|call| structured_ids.contains(call.id.as_str()))
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

fn active_round_text(message: &InternalMessage, structured_ids: &HashSet<String>) -> String {
    let mut parts = Vec::new();
    let content = content_text(message);
    if !content.is_empty() {
        parts.push(content);
    }
    parts.extend(
        message
            .tool_results
            .iter()
            .filter(|result| !structured_ids.contains(result.tool_call_id.as_str()))
            .map(tool_result_text),
    );
    parts.join("\n")
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
    parts.extend(message.tool_calls.iter().map(tool_call_text));
    parts.extend(message.tool_results.iter().map(tool_result_text));
    parts.join("\n")
}

fn tool_call_text(call: &crate::protocol::internal::InternalToolCall) -> String {
    format!("[Tool call {} ({})]\n{}", call.name, call.id, call.arguments_json())
}

fn tool_result_text(result: &InternalToolResult) -> String {
    let status = if result.is_error { " error" } else { "" };
    let content = value_text(&result.content);
    let content = if content.is_empty() { result.content.to_string() } else { content };
    format!("[Tool result {}{}]\n{}", result.tool_call_id, status, content)
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
    fn preserves_system_and_developer_instructions_in_current_prompt() {
        let mut request =
            request(vec![InternalMessage::new("user", Value::String("answer briefly".into()))]);
        request.system = Some("You are a precise assistant.".into());
        request.instructions = Some("Use concise wording.".into());

        let body = conversation_body(&request, &Credential::default(), "AI_EDITOR", "kiro");
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "[System instructions]\nYou are a precise assistant.\n\n[Developer instructions]\nUse concise wording.\n\nanswer briefly"
        );
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

    #[test]
    fn filters_unknown_duplicate_and_undeclared_tool_results() {
        let mut assistant = InternalMessage::new("assistant", Value::Null);
        assistant.tool_calls.push(call("call_a", "alpha"));
        assistant.tool_calls.push(call("call_b", "beta"));
        assistant.tool_calls.push(call("call_hidden", "undeclared"));
        let mut incomplete = call("call_incomplete", "alpha");
        incomplete.complete = false;
        assistant.tool_calls.push(incomplete);

        let mut results = InternalMessage::new("tool", Value::Null);
        results.tool_results = vec![
            InternalToolResult {
                tool_call_id: "call_b".into(),
                content: Value::String(String::new()),
                is_error: false,
            },
            InternalToolResult {
                tool_call_id: "unknown".into(),
                content: Value::String("orphan".into()),
                is_error: false,
            },
            InternalToolResult {
                tool_call_id: "call_a".into(),
                content: Value::Null,
                is_error: true,
            },
            InternalToolResult {
                tool_call_id: "call_b".into(),
                content: Value::String("duplicate".into()),
                is_error: false,
            },
            InternalToolResult {
                tool_call_id: "call_hidden".into(),
                content: Value::String("hidden result".into()),
                is_error: false,
            },
            InternalToolResult {
                tool_call_id: "call_incomplete".into(),
                content: Value::String("partial result".into()),
                is_error: false,
            },
        ];
        let body = conversation_body(
            &request(vec![
                InternalMessage::new("user", Value::String("question".into())),
                assistant,
                results,
            ]),
            &Credential::default(),
            "AI_EDITOR",
            "kiro",
        );

        let state = &body["conversationState"];
        let active =
            state["history"][1]["assistantResponseMessage"]["toolUses"].as_array().unwrap();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0]["toolUseId"], "call_a");
        assert_eq!(active[1]["toolUseId"], "call_b");
        let assistant_text =
            state["history"][1]["assistantResponseMessage"]["content"].as_str().unwrap();
        assert!(assistant_text.contains("call_hidden"));
        assert!(assistant_text.contains("call_incomplete"));

        let current = &state["currentMessage"]["userInputMessage"];
        let native_results = current["userInputMessageContext"]["toolResults"].as_array().unwrap();
        assert_eq!(native_results.len(), 2);
        assert_eq!(native_results[0]["toolUseId"], "call_b");
        assert_eq!(native_results[0]["content"][0]["text"], "(empty result)");
        assert_eq!(native_results[1]["toolUseId"], "call_a");
        assert_eq!(native_results[1]["status"], "error");
        let current_text = current["content"].as_str().unwrap();
        assert!(current_text.contains("unknown"));
        assert!(current_text.contains("call_hidden"));
        assert!(current_text.contains("call_incomplete"));
        assert!(!current_text.contains("duplicate"));
    }

    #[test]
    fn does_not_keep_a_partial_tool_round_structured() {
        let mut assistant = InternalMessage::new("assistant", Value::Null);
        assistant.tool_calls.push(call("call_a", "alpha"));
        assistant.tool_calls.push(call("call_b", "beta"));
        let body = conversation_body(
            &request(vec![
                InternalMessage::new("user", Value::String("question".into())),
                assistant,
                result("call_a", Value::String("only one result".into()), false),
            ]),
            &Credential::default(),
            "AI_EDITOR",
            "kiro",
        );
        let state = &body["conversationState"];
        assert!(state["history"][1]["assistantResponseMessage"]["toolUses"].is_null());
        assert!(
            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                .get("toolResults")
                .is_none()
        );
        assert!(
            state["history"][1]["assistantResponseMessage"]["content"]
                .as_str()
                .unwrap()
                .contains("call_a")
        );
    }
}
