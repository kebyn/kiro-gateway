use crate::{
    auth::{AuthMethod, Credential},
    endpoint::KiroEndpoint,
    error::AppError,
    protocol::internal::{InternalEvent, InternalRequest},
    transform::truncation::XmlLeakFilter,
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
                    if matches!(error, AppError::Integrity(_)) {
                        integrity.incomplete = true;
                    }
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
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("json"))
        {
            let body: serde_json::Value = response
                .json()
                .await
                .map_err(|e| AppError::Upstream(format!("invalid JSON upstream response: {e}")))?;
            let text = body
                .get("content")
                .or_else(|| body.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let usage = body.get("usage").map(|value| {
                crate::protocol::internal::Usage::new(
                    value
                        .get("inputTokens")
                        .or_else(|| value.get("input_tokens"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                    value
                        .get("outputTokens")
                        .or_else(|| value.get("output_tokens"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                )
            });
            let tool_calls = parse_json_tool_calls(&body);
            return Ok(crate::protocol::internal::InternalResponse {
                text,
                tool_calls,
                usage,
                stop_reason: body
                    .get("stopReason")
                    .or_else(|| body.get("stop_reason"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                ..Default::default()
            });
        }
        let mut stream = response.bytes_stream();
        let mut decoder = EventStreamDecoder::new();
        let mut tools = ToolCallAccumulator::new();
        let mut xml_filter = XmlLeakFilter::new();
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
                        output.text.push_str(&xml_filter.push(&text));
                    }
                    InternalEvent::ThinkingDelta { text } => output.thinking.push_str(&text),
                    InternalEvent::ToolCallStart { id, name } => {
                        tools.start(Some(&id), &name);
                    }
                    InternalEvent::ToolCallDelta { id, arguments, name } => {
                        if let Some(name) = name {
                            tools.start(Some(&id), &name);
                        }
                        tools.append(Some(&id), &arguments);
                    }
                    InternalEvent::ToolCallEnd { id, complete } => {
                        tools.finish_with_state(Some(&id), complete);
                    }
                    InternalEvent::Usage { usage } => output.usage = Some(usage),
                    InternalEvent::Stop { reason } => output.stop_reason = Some(reason),
                    InternalEvent::Error { message } => return Err(AppError::Upstream(message)),
                }
            }
        }
        decoder.finish().map_err(|e| AppError::Integrity(e.to_string()))?;
        output.tool_calls = tools.finish_all();
        if output.tool_calls.iter().any(|call| !call.complete) {
            output.incomplete = true;
        }
        Ok(output)
    }
}

fn parse_json_tool_calls(
    body: &serde_json::Value,
) -> Vec<crate::protocol::internal::InternalToolCall> {
    let items = body
        .get("toolUses")
        .or_else(|| body.get("tool_uses"))
        .or_else(|| body.get("toolCalls"))
        .or_else(|| body.get("tool_calls"))
        .and_then(serde_json::Value::as_array);
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = item
                .get("toolUseId")
                .or_else(|| item.get("tool_use_id"))
                .or_else(|| item.get("id"))
                .and_then(serde_json::Value::as_str)?
                .to_owned();
            let name = item
                .get("name")
                .or_else(|| item.get("toolName"))
                .and_then(serde_json::Value::as_str)?
                .to_owned();
            let arguments = item
                .get("input")
                .or_else(|| item.get("arguments"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let arguments = match arguments {
                serde_json::Value::String(raw) => {
                    serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw))
                }
                value => value,
            };
            let complete =
                item.get("complete").and_then(serde_json::Value::as_bool).unwrap_or(true);
            Some(crate::protocol::internal::InternalToolCall { id, name, arguments, complete })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_json_tool_calls;

    #[test]
    fn parses_parallel_json_tool_uses() {
        let calls = parse_json_tool_calls(&serde_json::json!({
            "toolUses": [
                {"toolUseId":"call_a","name":"alpha","input":r#"{"a":1}"#},
                {"toolUseId":"call_b","name":"beta","input":{"b":2},"complete":false}
            ]
        }));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].arguments["a"], 1);
        assert_eq!(calls[1].arguments["b"], 2);
        assert!(!calls[1].complete);
    }
}
