pub mod cli;
pub mod ide;

use crate::{
    auth::Credential,
    error::AppError,
    protocol::internal::{InternalMessage, InternalRequest, content_text},
};

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
    let split_at = request.messages.len().saturating_sub(1);
    let history: Vec<serde_json::Value> = request.messages[..split_at]
        .iter()
        .map(|message| message_to_history(message, origin, model_id))
        .collect();
    let current = serde_json::json!({"userInputMessage":{"content":request.last_user_text(),"modelId":model_id,"origin":origin,"userInputMessageContext":{"tools":request.tools}}});
    let conversation_id =
        request.conversation_id.clone().unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let state = serde_json::json!({"conversationId":conversation_id,"history":history,"currentMessage":current,"chatTriggerType":"MANUAL","agentTaskType":"vibe"});
    let mut body = serde_json::json!({"conversationState":state});
    if let Some(profile_arn) = &credential.profile_arn {
        body["profileArn"] = serde_json::Value::String(profile_arn.clone());
    }
    body
}

fn message_to_history(
    message: &InternalMessage,
    origin: &str,
    model_id: &str,
) -> serde_json::Value {
    match message.role.as_str() {
        "assistant" => {
            serde_json::json!({"assistantResponseMessage":{"content":content_text(message)}})
        }
        "tool" => {
            serde_json::json!({"toolResultMessage":{"content":content_text(message),"toolUseId":message.tool_call_id}})
        }
        _ => {
            serde_json::json!({"userInputMessage":{"content":content_text(message),"modelId":model_id,"origin":origin,"userInputMessageContext":{}}})
        }
    }
}
