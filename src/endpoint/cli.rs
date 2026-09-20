use crate::{
    auth::Credential,
    endpoint::{KiroEndpoint, conversation_body},
    error::AppError,
    protocol::internal::InternalRequest,
};
use uuid::Uuid;

pub const CLI_USER_AGENT: &str = "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.14474 os/linux lang/rust/1.92.0 m/F app/AmazonQ-For-CLI";
pub const CLI_USER_AGENT_WITH_VERSION: &str = "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.14474 os/linux lang/rust/1.92.0 md/appVersion-0.9.2 app/AmazonQ-For-CLI";

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
    fn api_url(&self, credential: &Credential) -> String {
        self.upstream_url.clone().unwrap_or_else(|| {
            format!("https://runtime.{}.kiro.dev/generateAssistantResponse", credential.api_region)
        })
    }
    fn transform_api_body(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> serde_json::Value {
        let mut body = conversation_body(request, credential, "KIRO_CLI", &request.model);
        if let Some(profile_arn) = &credential.profile_arn {
            body["profileArn"] = serde_json::Value::String(profile_arn.clone());
        }
        if let Some(state) =
            body.get_mut("conversationState").and_then(serde_json::Value::as_object_mut)
        {
            if let Some(conversation_id) = state.get("conversationId").cloned() {
                state.insert("rootConversationId".into(), conversation_id);
            }
            state.insert(
                "agentContinuationId".into(),
                serde_json::Value::String(Uuid::new_v4().to_string()),
            );
        }
        body["agentMode"] = serde_json::Value::String("vibe".into());
        if let Some(history) = body
            .get_mut("conversationState")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|state| state.get_mut("history"))
            .and_then(serde_json::Value::as_array_mut)
        {
            for message in history {
                if let Some(user_input) =
                    message.get_mut("userInputMessage").and_then(serde_json::Value::as_object_mut)
                {
                    user_input.remove("modelId");
                }
            }
        }
        body
    }
    fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        _credential: &Credential,
    ) -> reqwest::RequestBuilder {
        builder
            .header("x-amz-target", "KiroRuntimeService.GenerateAssistantResponse")
            .header("content-type", "application/x-amz-json-1.0")
            .header("x-amzn-codewhisperer-optout", "false")
            .header("x-amzn-kiro-client-attribution", "unrecognized")
            .header("x-kiro-attempt", "1;max=3")
            .header("x-amz-user-agent", CLI_USER_AGENT)
            .header("user-agent", CLI_USER_AGENT_WITH_VERSION)
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
    }
    fn classify_error(&self, status: reqwest::StatusCode, _body: &str) -> AppError {
        AppError::Upstream(format!("CLI endpoint returned {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::CliEndpoint;
    use crate::{
        auth::Credential,
        endpoint::KiroEndpoint,
        protocol::internal::{InternalMessage, InternalRequest},
    };
    use serde_json::Value;

    #[test]
    fn uses_runtime_cli_endpoint_by_default() {
        let endpoint = CliEndpoint::new(None);
        let credential = Credential { api_region: "eu-central-1".into(), ..Default::default() };
        assert_eq!(
            endpoint.api_url(&credential),
            "https://runtime.eu-central-1.kiro.dev/generateAssistantResponse"
        );
    }

    #[test]
    fn uses_cli_origin_and_removes_model_ids_from_history() {
        let endpoint = CliEndpoint::new(None);
        let request = InternalRequest {
            model: "gpt-5.6-luna".into(),
            messages: vec![
                InternalMessage::new("user", Value::String("old".into())),
                InternalMessage::new("user", Value::String("current".into())),
            ],
            system: None,
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            max_tokens: None,
            temperature: None,
            conversation_id: None,
            instructions: None,
        };
        let body = endpoint.transform_api_body(&request, &Credential::default());
        assert_eq!(
            body["conversationState"]["history"][0]["userInputMessage"]["origin"],
            "KIRO_CLI"
        );
        assert!(
            body["conversationState"]["history"][0]["userInputMessage"].get("modelId").is_none()
        );
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "gpt-5.6-luna"
        );
        assert_eq!(body["agentMode"], "vibe");
        assert_eq!(
            body["conversationState"]["rootConversationId"],
            body["conversationState"]["conversationId"]
        );
        assert!(
            body["conversationState"]["agentContinuationId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
    }

    #[test]
    fn includes_profile_arn_for_runtime_cli_requests() {
        let endpoint = CliEndpoint::new(None);
        let request = InternalRequest {
            model: "gpt-5.6-luna".into(),
            messages: vec![InternalMessage::new("user", Value::String("current".into()))],
            system: None,
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            max_tokens: None,
            temperature: None,
            conversation_id: None,
            instructions: None,
        };
        let credential = Credential {
            profile_arn: Some("arn:aws:codewhisperer:eu-central-1:123:profile/test".into()),
            ..Default::default()
        };
        let body = endpoint.transform_api_body(&request, &credential);
        assert_eq!(body["profileArn"], "arn:aws:codewhisperer:eu-central-1:123:profile/test");
    }
}
