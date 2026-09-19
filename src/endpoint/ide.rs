use crate::{
    auth::Credential,
    endpoint::{KiroEndpoint, conversation_body},
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
    fn api_url(&self, credential: &Credential) -> String {
        self.upstream_url.clone().unwrap_or_else(|| {
            format!("https://q.{}.amazonaws.com/generateAssistantResponse", credential.api_region)
        })
    }
    fn transform_api_body(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> serde_json::Value {
        conversation_body(request, credential, "AI_EDITOR", &request.model)
    }
    fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("x-amz-target", "AmazonCodeWhispererStreamingService.GenerateAssistantResponse")
            .header("content-type", "application/x-amz-json-1.0")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amz-user-agent", "kiro-gateway/0.1")
            .header("x-kiro-machine-id", &credential.machine_id)
            .header("x-amzn-kiro-agent-mode", "agent")
            .header("origin", "https://app.kiro.dev")
    }
    fn classify_error(&self, status: reqwest::StatusCode, body: &str) -> AppError {
        AppError::Upstream(format!(
            "IDE endpoint returned {status}: {}",
            body.chars().take(256).collect::<String>()
        ))
    }
}
