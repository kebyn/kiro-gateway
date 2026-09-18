use crate::{
    auth::{AuthMethod, Credential},
    endpoint::KiroEndpoint,
    error::AppError,
    protocol::internal::{
        InternalEvent, InternalRequest, InternalResponse, InternalToolCall, Usage,
    },
    transform::truncation::XmlLeakFilter,
    upstream::{
        error::UpstreamStreamError,
        event_stream::{EventStreamDecoder, decode_internal_events},
        integrity::StreamIntegrity,
        tool_state::ToolCallAccumulator,
    },
};
use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::{pin::Pin, sync::Arc};

pub type InternalEventStream = Pin<Box<dyn Stream<Item = Result<InternalEvent, AppError>> + Send>>;

pub struct UpstreamClient {
    client: Client,
    endpoint: Arc<dyn KiroEndpoint>,
}
impl UpstreamClient {
    pub fn new(client: Client, endpoint: Box<dyn KiroEndpoint>) -> Self {
        Self { client, endpoint: endpoint.into() }
    }

    /// Starts a live upstream event stream. The request is sent only when the
    /// returned stream is polled, which lets HTTP handlers emit protocol
    /// headers and their initial lifecycle event before the first upstream
    /// frame arrives.
    pub async fn event_stream(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> Result<InternalEventStream, AppError> {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let request = request.clone();
        let credential = credential.clone();
        Ok(Box::pin(stream! {
            let mut emitted = false;
            for attempt in 0..=1_u8 {
                let response = match send_once(&client, endpoint.as_ref(), &request, &credential).await {
                    Ok(response) => response,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let is_json = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.contains("json"));
                if is_json {
                    let body = match response.bytes().await {
                        Ok(body) => body,
                        Err(error) => {
                            yield Err(AppError::Upstream(error.to_string()));
                            return;
                        }
                    };
                    match json_events(&body) {
                        Ok(events) => {
                            for event in events {
                                yield Ok(event);
                            }
                            return;
                        }
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    }
                }

                let mut bytes = response.bytes_stream();
                let mut decoder = EventStreamDecoder::new();
                let mut retry = false;
                while let Some(chunk) = bytes.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            if !emitted && attempt == 0 {
                                retry = true;
                                break;
                            }
                            yield Err(AppError::Upstream(error.to_string()));
                            return;
                        }
                    };
                    let messages = match decoder.push(&chunk) {
                        Ok(messages) => messages,
                        Err(error) => {
                            if !emitted && attempt == 0 {
                                retry = true;
                                break;
                            }
                            yield Err(AppError::Integrity(error.to_string()));
                            return;
                        }
                    };
                    for message in messages {
                        let events = match decode_internal_events(&message) {
                            Ok(events) => events,
                            Err(UpstreamStreamError::Upstream(message)) => {
                                if !emitted && attempt == 0 {
                                    retry = true;
                                    break;
                                }
                                yield Err(AppError::Upstream(message));
                                return;
                            }
                            Err(error) => {
                                if !emitted && attempt == 0 {
                                    retry = true;
                                    break;
                                }
                                yield Err(AppError::Integrity(error.to_string()));
                                return;
                            }
                        };
                        for event in events {
                            // Empty metadata/context frames are intentionally
                            // not emitted by the decoder. Every yielded event
                            // is therefore observable protocol data.
                            emitted = true;
                            yield Ok(event);
                        }
                        if retry {
                            break;
                        }
                    }
                    if retry {
                        break;
                    }
                }
                if retry {
                    continue;
                }
                if let Err(error) = decoder.finish() {
                    if !emitted && attempt == 0 {
                        continue;
                    }
                    yield Err(AppError::Integrity(error.to_string()));
                    return;
                }
                if !emitted {
                    if attempt == 0 {
                        continue;
                    }
                    yield Err(AppError::Integrity("upstream stream was empty".into()));
                }
                return;
            }
        }))
    }

    /// Short compatibility alias for callers that refer to the upstream
    /// interface simply as a stream.
    pub async fn stream(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> Result<InternalEventStream, AppError> {
        self.event_stream(request, credential).await
    }

    pub async fn complete(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> Result<InternalResponse, AppError> {
        let mut events = self.event_stream(request, credential).await?;
        let mut accumulator = InternalEventAccumulator::new();
        while let Some(event) = events.next().await {
            accumulator.push(event?)?;
        }
        let output = accumulator.finish();
        if is_empty_stream(&output) {
            return Err(AppError::Integrity("upstream stream was empty".into()));
        }
        Ok(output)
    }
}

async fn send_once(
    client: &Client,
    endpoint: &dyn KiroEndpoint,
    request: &InternalRequest,
    credential: &Credential,
) -> Result<reqwest::Response, AppError> {
    let body = endpoint.transform_api_body(request, credential);
    let mut builder = client
        .post(endpoint.api_url(credential))
        .bearer_auth(
            credential.access_token.as_ref().map(|v| v.expose_secret()).unwrap_or_default(),
        )
        .json(&body)
        .header("x-amzn-codewhisperer-optout", "true")
        .header("accept", "application/vnd.amazon.eventstream, application/json");
    if matches!(credential.auth_method, AuthMethod::ApiKey) {
        builder = builder.header("tokentype", "API_KEY");
    } else if matches!(credential.auth_method, AuthMethod::Social) {
        builder = builder.header("TokenType", "EXTERNAL_IDP");
    }
    let response = endpoint
        .decorate_api(builder, credential)
        .send()
        .await
        .map_err(|error| AppError::Upstream(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(endpoint.classify_error(status, &body));
    }
    Ok(response)
}

/// Converts a complete JSON response into the same logical events produced by
/// the binary EventStream endpoint. JSON is necessarily finite, but downstream
/// protocol handlers still consume it through the one event path.
fn json_events(body: &[u8]) -> Result<Vec<InternalEvent>, AppError> {
    let body: Value = serde_json::from_slice(body)
        .map_err(|error| AppError::Upstream(format!("invalid JSON upstream response: {error}")))?;
    if let Some(error) = body.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .unwrap_or("upstream error")
            .to_owned();
        return Ok(vec![InternalEvent::Error { message }]);
    }
    let mut events = Vec::new();
    if let Some(text) = json_text(&body, &["content", "text"]).filter(|text| !text.is_empty()) {
        events.push(InternalEvent::TextDelta { text: text.to_owned() });
    }
    if let Some(text) = json_text(&body, &["thinking", "reasoning"]).filter(|text| !text.is_empty())
    {
        events.push(InternalEvent::ThinkingDelta { text: text.to_owned() });
    }
    for call in parse_json_tool_calls(&body) {
        let id = call.id.clone();
        let name = call.name.clone();
        let arguments = call.arguments_json();
        let complete = call.complete;
        events.push(InternalEvent::ToolCallStart { id: id.clone(), name });
        events.push(InternalEvent::ToolCallDelta { id: id.clone(), arguments, name: None });
        events.push(InternalEvent::ToolCallEnd { id, complete });
    }
    if let Some(usage) = body.get("usage") {
        events.push(InternalEvent::Usage {
            usage: Usage::new(
                usage
                    .get("inputTokens")
                    .or_else(|| usage.get("input_tokens"))
                    .or_else(|| usage.get("prompt_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                usage
                    .get("outputTokens")
                    .or_else(|| usage.get("output_tokens"))
                    .or_else(|| usage.get("completion_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            ),
        });
    }
    let reason = body
        .get("stopReason")
        .or_else(|| body.get("stop_reason"))
        .or_else(|| body.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("end_turn")
        .to_owned();
    // A JSON response with no visible fields is still a valid terminal
    // response when the upstream explicitly returned an object.
    events.push(InternalEvent::Stop { reason });
    Ok(events)
}

fn json_text(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        let value = value.get(*name)?;
        match value {
            Value::String(text) => Some(text.clone()),
            Value::Object(object) => object
                .get("text")
                .or_else(|| object.get("content"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            Value::Array(items) => {
                let text = items
                    .iter()
                    .filter_map(|item| {
                        item.as_str()
                            .or_else(|| item.get("text").and_then(Value::as_str))
                            .or_else(|| item.get("content").and_then(Value::as_str))
                    })
                    .collect::<Vec<_>>()
                    .join("");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        }
    })
}

/// Shared state machine used by complete responses and by all streaming HTTP
/// handlers. It preserves tool ordering and the cross-chunk XML filter.
pub struct InternalEventAccumulator {
    output: InternalResponse,
    tools: ToolCallAccumulator,
    xml_filter: XmlLeakFilter,
    integrity: StreamIntegrity,
}

impl InternalEventAccumulator {
    pub fn new() -> Self {
        Self {
            output: InternalResponse::default(),
            tools: ToolCallAccumulator::new(),
            xml_filter: XmlLeakFilter::new(),
            integrity: StreamIntegrity::default(),
        }
    }

    pub fn push(&mut self, event: InternalEvent) -> Result<(), AppError> {
        apply_internal_event(
            event,
            &mut self.output,
            &mut self.tools,
            &mut self.xml_filter,
            &mut self.integrity,
        )
    }

    pub fn finish(mut self) -> InternalResponse {
        finish_stream_response(self.output, &mut self.tools)
    }

    pub fn snapshot(&self) -> InternalResponse {
        let output = self.output.clone();
        let mut tools = self.tools.clone();
        finish_stream_response(output, &mut tools)
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
        InternalEvent::ToolCallDelta { id, arguments, name } => {
            if let Some(name) = name {
                tools.start(Some(&id), &name);
            }
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

fn is_empty_stream(response: &crate::protocol::internal::InternalResponse) -> bool {
    response.text.is_empty()
        && response.thinking.is_empty()
        && response.tool_calls.is_empty()
        && response.stop_reason.is_none()
}

fn parse_json_tool_calls(
    body: &serde_json::Value,
) -> Vec<crate::protocol::internal::InternalToolCall> {
    let items = body
        .get("toolUses")
        .or_else(|| body.get("tool_uses"))
        .or_else(|| body.get("toolCalls"))
        .or_else(|| body.get("tool_calls"))
        .or_else(|| body.get("output"))
        .and_then(serde_json::Value::as_array);
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            if item
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind != "function_call" && kind != "tool_use")
            {
                return None;
            }
            let id = item
                .get("toolUseId")
                .or_else(|| item.get("tool_use_id"))
                .or_else(|| item.get("call_id"))
                .or_else(|| item.get("id"))
                .and_then(serde_json::Value::as_str)?
                .to_owned();
            let name = item
                .get("name")
                .or_else(|| item.get("toolName"))
                .or_else(|| item.get("function").and_then(|function| function.get("name")))
                .and_then(serde_json::Value::as_str)?
                .to_owned();
            let arguments = item
                .get("input")
                .or_else(|| item.get("arguments"))
                .or_else(|| item.get("function").and_then(|function| function.get("arguments")))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let (arguments, arguments_complete) = match arguments {
                serde_json::Value::String(raw) if raw.trim().is_empty() => {
                    (serde_json::json!({}), true)
                }
                serde_json::Value::String(raw) => {
                    match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(value) if value.is_object() => (value, true),
                        Ok(value) => (value, false),
                        Err(_) => (serde_json::Value::String(raw), false),
                    }
                }
                serde_json::Value::Object(object) => (serde_json::Value::Object(object), true),
                value => (value, false),
            };
            let complete =
                item.get("complete").and_then(serde_json::Value::as_bool).unwrap_or(true)
                    && arguments_complete
                    && !matches!(
                        item.get("status").and_then(serde_json::Value::as_str),
                        Some("incomplete" | "failed" | "cancelled")
                    );
            Some(crate::protocol::internal::InternalToolCall { id, name, arguments, complete })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        apply_internal_event, finish_stream_response, is_empty_stream, json_events,
        parse_json_tool_calls,
    };
    use crate::{
        auth::{AuthMethod, Credential, SecretString},
        endpoint::{EndpointKind, endpoint_for},
        protocol::internal::{InternalEvent, InternalRequest, InternalResponse},
        transform::truncation::XmlLeakFilter,
        upstream::{
            event_stream::{EventStreamDecoder, decode_internal_events},
            integrity::StreamIntegrity,
            tool_state::ToolCallAccumulator,
        },
    };
    use crc::{CRC_32_ISO_HDLC, Crc};
    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

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

    #[tokio::test]
    async fn retries_truncated_attempt_before_emitting_any_event() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                server_attempts.fetch_add(1, Ordering::SeqCst);
                let body = if attempt == 0 {
                    b"bad".to_vec()
                } else {
                    let mut body = event_frame("assistantResponseEvent", json!({"content":"ok"}));
                    body.extend(event_frame("metadataEvent", json!({"stopReason":"end_turn"})));
                    body
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let upstream_url = format!("http://{address}");
        let client = super::UpstreamClient::new(
            reqwest::Client::new(),
            endpoint_for(EndpointKind::Ide, Some(upstream_url.as_str())),
        );
        let credential = Credential {
            auth_method: AuthMethod::ApiKey,
            access_token: Some(SecretString::new("token")),
            ..Default::default()
        };
        let request = InternalRequest {
            model: "kiro".into(),
            messages: vec![crate::protocol::internal::InternalMessage::new(
                "user",
                Value::String("hello".into()),
            )],
            system: None,
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            max_tokens: None,
            temperature: None,
            conversation_id: None,
            instructions: None,
        };
        let mut events = client.event_stream(&request, &credential).await.unwrap();
        let mut collected = Vec::new();
        while let Some(event) = events.next().await {
            collected.push(event.unwrap());
        }
        server.await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            matches!(&collected[..], [InternalEvent::TextDelta { text }, InternalEvent::Stop { reason }] if text == "ok" && reason == "end_turn")
        );
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
    fn marks_invalid_json_tool_arguments_incomplete() {
        let calls = parse_json_tool_calls(&serde_json::json!({
            "toolUses": [{
                "toolUseId":"call_partial",
                "name":"lookup",
                "input":"{\"query\":"
            }]
        }));
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].complete);
        assert_eq!(calls[0].arguments, serde_json::json!("{\"query\":"));
    }

    #[test]
    fn adapts_json_response_to_ordered_internal_events() {
        let events = json_events(
            br#"{"content":"answer","thinking":"plan","toolUses":[{"toolUseId":"call_1","name":"lookup","input":"{\"id\":1}"}],"usage":{"inputTokens":2,"outputTokens":3},"stopReason":"end_turn"}"#,
        )
        .unwrap();
        assert!(matches!(events[0], InternalEvent::TextDelta { .. }));
        assert!(matches!(events[1], InternalEvent::ThinkingDelta { .. }));
        assert!(matches!(events[2], InternalEvent::ToolCallStart { .. }));
        assert!(matches!(events[3], InternalEvent::ToolCallDelta { .. }));
        assert!(matches!(events[4], InternalEvent::ToolCallEnd { .. }));
        assert!(matches!(events[5], InternalEvent::Usage { .. }));
        assert!(matches!(events[6], InternalEvent::Stop { .. }));
    }

    #[test]
    fn adapts_json_upstream_error_to_error_event() {
        let events = json_events(br#"{"error":{"message":"overloaded"}}"#).unwrap();
        assert!(
            matches!(&events[..], [InternalEvent::Error { message }] if message == "overloaded")
        );
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

    #[test]
    fn rejects_a_clean_but_empty_stream_without_terminal_signal() {
        assert!(is_empty_stream(&InternalResponse::default()));
        assert!(!is_empty_stream(&InternalResponse {
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        }));
    }
}
