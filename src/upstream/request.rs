use crate::{
    auth::{AuthMethod, Credential},
    endpoint::KiroEndpoint,
    error::AppError,
    protocol::internal::{InternalEvent, InternalRequest},
    upstream::{
        event_stream::{EventStreamDecoder, decode_internal_event},
        integrity::{RetryDecision, StreamIntegrity},
        tool_state::ToolCallAccumulator,
    },
};
use futures_util::StreamExt;
use reqwest::Client;

pub struct UpstreamClient {
    client: Client,
    endpoint: Box<dyn KiroEndpoint>,
}
impl UpstreamClient {
    pub fn new(client: Client, endpoint: Box<dyn KiroEndpoint>) -> Self {
        Self { client, endpoint }
    }
    pub async fn complete(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> Result<crate::protocol::internal::InternalResponse, AppError> {
        let mut integrity = StreamIntegrity::default();
        for attempt in 0..=1 {
            integrity.attempts = attempt;
            match self.complete_once(request, credential, &mut integrity).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    if integrity.should_retry() == RetryDecision::Retry {
                        continue;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        Err(AppError::Integrity("upstream retry exhausted".into()))
    }
    async fn complete_once(
        &self,
        request: &InternalRequest,
        credential: &Credential,
        integrity: &mut StreamIntegrity,
    ) -> Result<crate::protocol::internal::InternalResponse, AppError> {
        let body = self.endpoint.transform_api_body(request, credential);
        let mut builder = self
            .client
            .post(self.endpoint.api_url(credential))
            .bearer_auth(
                credential.access_token.as_ref().map(|v| v.expose_secret()).unwrap_or_default(),
            )
            .json(&body);
        builder = builder.header("x-amzn-codewhisperer-optout", "true");
        if matches!(credential.auth_method, AuthMethod::ApiKey) {
            builder = builder.header("tokentype", "API_KEY");
        } else if matches!(credential.auth_method, AuthMethod::Social) {
            builder = builder.header("TokenType", "EXTERNAL_IDP");
        }
        let response = self
            .endpoint
            .decorate_api(builder, credential)
            .send()
            .await
            .map_err(|e| AppError::Upstream(e.to_string()))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(self.endpoint.classify_error(status, &body));
        }
        let mut stream = response.bytes_stream();
        let mut decoder = EventStreamDecoder::new();
        let mut tools = ToolCallAccumulator::new();
        let mut output = crate::protocol::internal::InternalResponse::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AppError::Upstream(e.to_string()))?;
            let messages = decoder.push(&chunk).map_err(|e| AppError::Integrity(e.to_string()))?;
            for message in messages {
                match decode_internal_event(&message)
                    .map_err(|e| AppError::Integrity(e.to_string()))?
                {
                    InternalEvent::TextDelta { text } => {
                        integrity.record_emission();
                        output.text.push_str(&text);
                    }
                    InternalEvent::ThinkingDelta { text } => output.thinking.push_str(&text),
                    InternalEvent::ToolCallStart { id, name } => {
                        tools.start(Some(&id), &name);
                    }
                    InternalEvent::ToolCallDelta { id, arguments } => {
                        tools.append(Some(&id), &arguments);
                    }
                    InternalEvent::ToolCallEnd { id, complete } => {
                        if let Some(mut call) = tools.finish(Some(&id)) {
                            call.complete = complete;
                            output.tool_calls.push(call);
                        }
                    }
                    InternalEvent::Usage { usage } => output.usage = Some(usage),
                    InternalEvent::Stop { reason } => output.stop_reason = Some(reason),
                    InternalEvent::Error { message } => return Err(AppError::Upstream(message)),
                }
            }
        }
        decoder.finish().map_err(|e| AppError::Integrity(e.to_string()))?;
        if tools.incomplete() {
            output.incomplete = true;
            if output.tool_calls.is_empty() {
                output.tool_calls = tools.calls();
            }
        }
        Ok(output)
    }
}
