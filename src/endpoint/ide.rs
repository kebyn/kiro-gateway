use crate::{
    auth::Credential,
    endpoint::{EndpointKind, KiroEndpoint},
    error::AppError,
    protocol::internal::InternalRequest,
};

#[derive(Clone, Debug)]
pub struct IdeEndpoint {
    upstream_url: Option<String>,
}
impl IdeEndpoint {
    pub fn new(upstream_url: Option<String>) -> Self {
        Self { upstream_url }
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn kind(&self) -> EndpointKind {
        EndpointKind::Ide
    }
    fn api_url(&self, credential: &Credential) -> String {
        self.upstream_url.clone().unwrap_or_else(|| {
            format!("https://q.{}.amazonaws.com/generateAssistantResponse", credential.api_region)
        })
    }
    fn mcp_url(&self, credential: &Credential) -> String {
        self.upstream_url
            .clone()
            .unwrap_or_else(|| format!("https://q.{}.amazonaws.com/mcp", credential.api_region))
    }
    fn transform_api_body(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> serde_json::Value {
        serde_json::json!({"conversationState": {"history": request.messages, "currentMessage": request.last_user_text()}, "profileArn": credential.profile_arn, "conversationId": request.conversation_id, "mode": "agent"})
    }
    fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("x-amz-user-agent", "kiro-gateway-rs/0.1")
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
            "IDE endpoint returned {status}: {}",
            body.chars().take(256).collect::<String>()
        ))
    }
}
