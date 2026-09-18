use crate::{
    auth::Credential,
    endpoint::{EndpointKind, KiroEndpoint},
    error::AppError,
    protocol::internal::InternalRequest,
};

#[derive(Clone, Debug)]
pub struct CliEndpoint {
    upstream_url: Option<String>,
}
impl CliEndpoint {
    pub fn new(upstream_url: Option<String>) -> Self {
        Self { upstream_url }
    }
}

impl KiroEndpoint for CliEndpoint {
    fn kind(&self) -> EndpointKind {
        EndpointKind::Cli
    }
    fn api_url(&self, credential: &Credential) -> String {
        self.upstream_url.clone().unwrap_or_else(|| {
            format!(
                "https://codewhisperer.{}.amazonaws.com/generateAssistantResponse",
                credential.api_region
            )
        })
    }
    fn mcp_url(&self, credential: &Credential) -> String {
        self.upstream_url.clone().unwrap_or_else(|| {
            format!("https://codewhisperer.{}.amazonaws.com/mcp", credential.api_region)
        })
    }
    fn transform_api_body(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> serde_json::Value {
        serde_json::json!({"conversationState": {"history": request.messages, "currentMessage": request.last_user_text()}, "profileArn": credential.profile_arn, "clientName": "kiro-cli", "machineId": credential.machine_id, "conversationId": request.conversation_id})
    }
    fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("x-amz-user-agent", "kiro-cli/1.0 kiro-gateway-rs/0.1")
            .header("x-kiro-machine-id", &credential.machine_id)
            .header("origin", "https://app.kiro.dev")
    }
    fn decorate_mcp(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        self.decorate_api(builder, credential)
            .header("accept", "application/vnd.amazon.eventstream")
    }
    fn classify_error(&self, status: reqwest::StatusCode, body: &str) -> AppError {
        AppError::Upstream(format!(
            "CLI endpoint returned {status}: {}",
            body.chars().take(256).collect::<String>()
        ))
    }
}
