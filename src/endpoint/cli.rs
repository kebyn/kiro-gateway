use crate::{
    auth::Credential,
    endpoint::{EndpointKind, KiroEndpoint, conversation_body},
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
        let mut body = conversation_body(request, credential, "KIRO_CLI", "auto");
        body["clientName"] = serde_json::Value::String("kiro-cli".into());
        body["machineId"] = serde_json::Value::String(credential.machine_id.clone());
        body
    }
    fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("x-amz-target", "AmazonCodeWhispererStreamingService.GenerateAssistantResponse")
            .header("content-type", "application/x-amz-json-1.0")
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
