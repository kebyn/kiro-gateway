use crate::{
    auth::{AuthMethod, Credential},
    endpoint::KiroEndpoint,
    error::AppError,
    protocol::internal::{InternalEvent, InternalRequest},
    transform::truncation::XmlLeakFilter,
    upstream::{
        error::UpstreamStreamError,
        event_stream::{EventStreamDecoder, decode_internal_events},
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
                let events = decode_internal_events(&message).map_err(|error| match error {
                    UpstreamStreamError::Upstream(message) => AppError::Upstream(message),
                    error => AppError::Integrity(error.to_string()),
                })?;
                for event in events {
                    apply_internal_event(
                        event,
                        &mut output,
                        &mut tools,
                        &mut xml_filter,
                        integrity,
                    )?;
                }
            }
        }
        decoder.finish().map_err(|e| AppError::Integrity(e.to_string()))?;
        Ok(finish_stream_response(output, &mut tools))
    }
}

fn apply_internal_event(
    event: InternalEvent,
    output: &mut crate::protocol::internal::InternalResponse,
    tools: &mut ToolCallAccumulator,
    xml_filter: &mut XmlLeakFilter,
    integrity: &mut StreamIntegrity,
) -> Result<(), AppError> {
    match event {
        InternalEvent::TextDelta { text } => {
            integrity.record_emission();
            output.text.push_str(&xml_filter.push(&text));
        }
        InternalEvent::ThinkingDelta { text } => {
            integrity.record_emission();
            output.thinking.push_str(&text);
        }
        InternalEvent::ToolCallStart { id, name } => {
            tools.start(Some(&id), &name);
        }
        InternalEvent::ToolCallDelta { id, arguments } => {
            tools.append(Some(&id), &arguments);
        }
        InternalEvent::ToolCallEnd { id, complete } => {
            if tools.finish_with_state(Some(&id), complete).is_some_and(|call| call.complete) {
                integrity.completed = true;
            }
        }
        InternalEvent::Usage { usage } => output.usage = Some(usage),
        InternalEvent::Stop { reason } => {
            integrity.completed = true;
            output.stop_reason = Some(reason);
        }
        InternalEvent::Error { message } => return Err(AppError::Upstream(message)),
    }
    Ok(())
}

fn finish_stream_response(
    mut output: crate::protocol::internal::InternalResponse,
    tools: &mut ToolCallAccumulator,
) -> crate::protocol::internal::InternalResponse {
    output.tool_calls = tools.finish_all();
    let truncated_without_terminal = output.stop_reason.is_none()
        && output.tool_calls.is_empty()
        && (!output.text.is_empty() || !output.thinking.is_empty());
    let stopped_incomplete = output.stop_reason.as_deref().is_some_and(|reason| {
        matches!(
            reason.trim().to_ascii_lowercase().as_str(),
            "max_tokens"
                | "max_output_tokens"
                | "length"
                | "model_context_window_exceeded"
                | "context_window_exceeded"
                | "refusal"
                | "content_filter"
                | "content_filtered"
                | "guardrail_intervened"
        )
    });
    output.incomplete = output.tool_calls.iter().any(|call| !call.complete)
        || truncated_without_terminal
        || stopped_incomplete;
    output
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
    use super::{apply_internal_event, finish_stream_response, parse_json_tool_calls};
    use crate::{
        protocol::internal::InternalResponse,
        transform::truncation::XmlLeakFilter,
        upstream::{
            event_stream::{EventStreamDecoder, decode_internal_events},
            integrity::StreamIntegrity,
            tool_state::ToolCallAccumulator,
        },
    };
    use crc::{CRC_32_ISO_HDLC, Crc};
    use serde_json::{Value, json};

    const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

    fn event_frame(event_type: &str, payload: Value) -> Vec<u8> {
        let payload = serde_json::to_vec(&payload).unwrap();
        let mut headers = Vec::new();
        headers.push(11);
        headers.extend_from_slice(b":event-type");
        headers.push(7);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());
        let total = 16 + headers.len() + payload.len();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&CRC32.checksum(&frame).to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&CRC32.checksum(&frame).to_be_bytes());
        frame
    }

    fn response_from_events(events: Vec<(&str, Value)>) -> InternalResponse {
        let bytes = events
            .into_iter()
            .flat_map(|(kind, payload)| event_frame(kind, payload))
            .collect::<Vec<_>>();
        let mut decoder = EventStreamDecoder::new();
        let mut tools = ToolCallAccumulator::new();
        let mut filter = XmlLeakFilter::new();
        let mut integrity = StreamIntegrity::default();
        let mut output = InternalResponse::default();
        for message in decoder.push(&bytes).unwrap() {
            for event in decode_internal_events(&message).unwrap() {
                apply_internal_event(event, &mut output, &mut tools, &mut filter, &mut integrity)
                    .unwrap();
            }
        }
        decoder.finish().unwrap();
        finish_stream_response(output, &mut tools)
    }

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

    #[test]
    fn event_stream_rebinds_id_and_honors_stop_aliases() {
        for stop_key in ["stop", "isStop", "done"] {
            let mut final_fragment =
                json!({"toolUseID":"call_real","toolName":"lookup","input":"\"rust\"}"});
            final_fragment[stop_key] = json!(true);
            let response = response_from_events(vec![
                ("toolUseEvent", json!({"tool_name":"lookup","input":"{\"query\":"})),
                ("toolUseEvent", final_fragment),
            ]);
            assert_eq!(response.tool_calls.len(), 1);
            assert_eq!(response.tool_calls[0].id, "call_real");
            assert_eq!(response.tool_calls[0].name, "lookup");
            assert_eq!(response.tool_calls[0].arguments["query"], "rust");
            assert!(response.tool_calls[0].complete);
            assert!(!response.incomplete);
        }
    }

    #[test]
    fn event_stream_rebinds_real_id_without_repeated_name() {
        let response = response_from_events(vec![
            ("toolUseEvent", json!({"name":"lookup","input":"{\"query\":"})),
            ("toolUseEvent", json!({"toolUseId":"call_real","input":"\"rust\"}","stop":true})),
        ]);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_real");
        assert_eq!(response.tool_calls[0].name, "lookup");
        assert_eq!(response.tool_calls[0].arguments["query"], "rust");
    }

    #[test]
    fn event_stream_preserves_interleaved_tool_inputs_and_order() {
        let response = response_from_events(vec![
            ("toolUseEvent", json!({"toolUseId":"call_a","name":"alpha","input":"{\"value\":"})),
            ("toolUseEvent", json!({"tool_use_id":"call_b","name":"beta","input":"{\"value\":"})),
            ("toolUseEvent", json!({"id":"call_a","input":"1}","stop":true})),
            ("toolUseEvent", json!({"id":"call_b","input":"2}","stop":true})),
        ]);
        assert_eq!(
            response.tool_calls.iter().map(|call| call.id.as_str()).collect::<Vec<_>>(),
            ["call_a", "call_b"]
        );
        assert_eq!(response.tool_calls[0].arguments["value"], 1);
        assert_eq!(response.tool_calls[1].arguments["value"], 2);
    }

    #[test]
    fn event_stream_name_change_and_orphan_fragment_do_not_pollute_tools() {
        let response = response_from_events(vec![
            ("toolUseEvent", json!({"id":"orphan","input":"ignored"})),
            ("toolUseEvent", json!({"id":"first","name":"alpha","input":{"a":1}})),
            ("toolUseEvent", json!({"name":"beta","input":{"b":2}})),
        ]);
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "first");
        assert_eq!(response.tool_calls[0].arguments, json!({"a":1}));
        assert_eq!(response.tool_calls[1].name, "beta");
        assert_eq!(response.tool_calls[1].arguments, json!({"b":2}));
        assert!(response.tool_calls.iter().all(|call| call.name != "unknown"));
    }

    #[test]
    fn event_stream_flushes_eof_and_marks_truncated_json_incomplete() {
        let complete = response_from_events(vec![(
            "toolUseEvent",
            json!({"toolUseId":"call","name":"lookup","input":{"query":"rust"}}),
        )]);
        assert!(complete.tool_calls[0].complete);
        assert!(!complete.incomplete);

        let incomplete = response_from_events(vec![(
            "toolUseEvent",
            json!({"toolUseId":"call","name":"lookup","input":"{\"query\":"}),
        )]);
        assert!(!incomplete.tool_calls[0].complete);
        assert!(incomplete.incomplete);
        assert_eq!(incomplete.tool_calls[0].arguments, json!("{\"query\":"));
    }

    #[test]
    fn event_stream_accepts_stopped_tool_with_empty_object_input() {
        let response = response_from_events(vec![(
            "toolUseEvent",
            json!({"toolUseId":"call","name":"noop","input":{},"stop":true}),
        )]);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].arguments, json!({}));
        assert!(response.tool_calls[0].complete);
        assert!(!response.incomplete);
    }

    #[test]
    fn event_stream_keeps_reasoning_and_stop_reason() {
        for key in ["stopReason", "stop_reason"] {
            let mut metadata = json!({});
            metadata[key] = json!("end_turn");
            let response = response_from_events(vec![
                ("reasoningContentEvent", json!({"text":"thinking"})),
                ("assistantResponseEvent", json!({"content":"answer"})),
                ("metadataEvent", metadata),
            ]);
            assert_eq!(response.thinking, "thinking");
            assert_eq!(response.text, "answer");
            assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));
            assert!(!response.incomplete);
        }
    }
}
