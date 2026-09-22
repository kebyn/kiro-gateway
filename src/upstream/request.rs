use crate::{
    auth::{AuthMethod, Credential},
    endpoint::{EndpointAdapter, EndpointPolicy, endpoint_for},
    error::AppError,
    protocol::internal::{InternalEvent, InternalRequest, InternalResponse, Usage},
    transform::truncation::XmlLeakFilter,
    upstream::{
        error::UpstreamStreamError,
        event_stream::{EventStreamDecoder, decode_internal_events},
        integrity::{RetryDecision, StreamIntegrity},
        tool_state::ToolCallAccumulator,
    },
};
use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::time::Instant;
use std::{collections::HashSet, pin::Pin};

pub type InternalEventStream = Pin<Box<dyn Stream<Item = Result<InternalEvent, AppError>> + Send>>;

pub struct UpstreamClient {
    client: Client,
    endpoint_policy: EndpointPolicy,
    upstream_url: Option<String>,
    max_body_bytes: usize,
}

impl UpstreamClient {
    #[allow(dead_code)]
    pub fn with_policy(
        client: Client,
        policy: EndpointPolicy,
        upstream_url: Option<String>,
    ) -> Self {
        Self::with_policy_and_limit(client, policy, upstream_url, 16 * 1024 * 1024)
    }

    pub fn with_policy_and_limit(
        client: Client,
        policy: EndpointPolicy,
        upstream_url: Option<String>,
        max_body_bytes: usize,
    ) -> Self {
        Self { client, endpoint_policy: policy, upstream_url, max_body_bytes }
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
        let endpoint_policy = self.endpoint_policy;
        let upstream_url = self.upstream_url.clone();
        let max_body_bytes = self.max_body_bytes;
        let request = request.clone();
        let credential = credential.clone();
        let endpoint_kind = endpoint_policy.resolve(&credential.endpoint)?;
        Ok(Box::pin(stream! {
            for attempt in 0..=1_u8 {
                let started = Instant::now();
                let endpoint = endpoint_for(
                    endpoint_kind,
                    upstream_url.as_deref(),
                );
                let response = match send_once(&client, &endpoint, &request, &credential).await {
                    Ok(response) => response,
                    Err(SendOnceError::Transport(error)) if attempt == 0 => {
                        tracing::debug!(
                            model = %request.model,
                            attempt,
                            retry_reason = "request_transport",
                            error = %error,
                            "retrying upstream request before emitting events"
                        );
                        continue;
                    }
                    Err(error) => {
                        yield Err(error.into_app_error());
                        return;
                    }
                };
                tracing::debug!(
                    model = %request.model,
                    attempt,
                    content_type = ?response.headers().get(reqwest::header::CONTENT_TYPE),
                    "received upstream response"
                );
                let is_json = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.contains("json"));
                if is_json {
                    if response
                        .content_length()
                        .is_some_and(|length| length > max_body_bytes as u64)
                    {
                        yield Err(AppError::Upstream("upstream response exceeds configured body limit".into()));
                        return;
                    }
                    let body = match response.bytes().await {
                        Ok(body) => body,
                        Err(error) if attempt == 0 => {
                            tracing::debug!(
                                model = %request.model,
                                attempt,
                                retry_reason = "response_body_transport",
                                error = %error,
                                "retrying upstream request before emitting events"
                            );
                            continue;
                        }
                        Err(error) => {
                            yield Err(AppError::Upstream(error.to_string()));
                            return;
                        }
                    };
                    if body.len() > max_body_bytes {
                        yield Err(AppError::Upstream("upstream response exceeds configured body limit".into()));
                        return;
                    }
                    match json_events(&body) {
                        Ok(events) => {
                            tracing::debug!(
                                model = %request.model,
                                events = events.len(),
                                stream_end_status = "completed",
                                upstream_ms = started.elapsed().as_millis() as u64,
                                "upstream JSON response ended"
                            );
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
                let mut integrity = StreamIntegrity { attempts: attempt, ..Default::default() };
                let mut body_bytes = 0_usize;
                let mut event_count = 0_usize;
                let mut retry = false;
                while let Some(chunk) = bytes.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            integrity.incomplete = true;
                            if matches!(integrity.should_retry(), RetryDecision::Retry) {
                                tracing::debug!(
                                    model = %request.model,
                                    attempt,
                                    retry_reason = "event_stream_transport",
                                    error = %error,
                                    "retrying upstream request before emitting events"
                                );
                                retry = true;
                                break;
                            }
                            yield Err(AppError::Upstream(error.to_string()));
                            return;
                        }
                    };
                    body_bytes = body_bytes.saturating_add(chunk.len());
                    if body_bytes > max_body_bytes {
                        yield Err(AppError::Upstream("upstream event stream exceeds configured body limit".into()));
                        return;
                    }
                    let messages = match decoder.push(&chunk) {
                        Ok(messages) => messages,
                        Err(error) => {
                            integrity.incomplete = true;
                            if matches!(integrity.should_retry(), RetryDecision::Retry) {
                                tracing::debug!(
                                    model = %request.model,
                                    attempt,
                                    retry_reason = "event_stream_decode",
                                    error = %error,
                                    "retrying upstream request before emitting events"
                                );
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
                                yield Err(AppError::Upstream(message));
                                return;
                            }
                            Err(error) => {
                                integrity.incomplete = true;
                                if matches!(integrity.should_retry(), RetryDecision::Retry) {
                                    tracing::debug!(
                                        model = %request.model,
                                        attempt,
                                        retry_reason = "event_decode",
                                        error = %error,
                                        "retrying upstream request before emitting events"
                                    );
                                    retry = true;
                                    break;
                                }
                                yield Err(AppError::Integrity(error.to_string()));
                                return;
                            }
                        };
                        for event in events {
                            event_count += 1;
                            if event_marks_completion(&event) {
                                integrity.completed = true;
                            }
                            // Empty metadata/context frames are intentionally
                            // not emitted by the decoder. Every yielded event
                            // is therefore observable protocol data.
                            integrity.record_emission();
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
                    integrity.incomplete = true;
                    if matches!(integrity.should_retry(), RetryDecision::Retry) {
                        tracing::debug!(
                            model = %request.model,
                            attempt,
                            retry_reason = "truncated_event_stream",
                            error = %error,
                            "retrying upstream request before emitting events"
                        );
                        continue;
                    }
                    yield Err(AppError::Integrity(error.to_string()));
                    return;
                }
                if !integrity.emitted_any {
                    integrity.incomplete = true;
                    if matches!(integrity.should_retry(), RetryDecision::Retry) {
                        tracing::debug!(
                            model = %request.model,
                            attempt,
                            retry_reason = "empty_event_stream",
                            "retrying upstream request before emitting events"
                        );
                        continue;
                    }
                    yield Err(AppError::Integrity("upstream stream was empty".into()));
                    return;
                }
                if !integrity.completed {
                    integrity.incomplete = true;
                    event_count += 1;
                    yield Ok(InternalEvent::Stop { reason: "stream_incomplete".into() });
                }
                tracing::debug!(
                    model = %request.model,
                    attempt,
                    events = event_count,
                    completed = integrity.completed,
                    stream_end_status = if integrity.completed { "completed" } else { "incomplete" },
                    upstream_ms = started.elapsed().as_millis() as u64,
                    "upstream event stream ended"
                );
                return;
            }
        }))
    }
}

enum SendOnceError {
    Transport(String),
    Application(AppError),
}

impl SendOnceError {
    fn into_app_error(self) -> AppError {
        match self {
            Self::Transport(message) => AppError::Upstream(message),
            Self::Application(error) => error,
        }
    }
}

async fn send_once(
    client: &Client,
    endpoint: &EndpointAdapter,
    request: &InternalRequest,
    credential: &Credential,
) -> Result<reqwest::Response, SendOnceError> {
    let body = endpoint.transform_api_body(request, credential);
    tracing::debug!(model = %request.model, "sending upstream request");
    let mut builder = client
        .post(endpoint.api_url(credential))
        .bearer_auth(
            credential.access_token.as_ref().map(|v| v.expose_secret()).unwrap_or_default(),
        )
        .json(&body)
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
        .map_err(|error| SendOnceError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map(|body| String::from_utf8_lossy(&body[..body.len().min(4096)]).into_owned())
            .unwrap_or_default();
        return Err(SendOnceError::Application(endpoint.classify_error(status, &body)));
    }
    Ok(response)
}

fn event_marks_completion(event: &InternalEvent) -> bool {
    matches!(
        event,
        InternalEvent::Stop { reason }
            if !matches!(
                reason.trim().to_ascii_lowercase().as_str(),
                "incomplete" | "stream_incomplete" | "upstream_disconnect"
            )
    ) || matches!(event, InternalEvent::ToolCallEnd { complete: true, .. })
}

/// Converts a complete JSON response into the same logical events produced by
/// the binary EventStream endpoint. JSON is necessarily finite, but downstream
/// protocol handlers still consume it through the one event path.
fn json_events(body: &[u8]) -> Result<Vec<InternalEvent>, AppError> {
    let body: Value = serde_json::from_slice(body)
        .map_err(|error| AppError::Upstream(format!("invalid JSON upstream response: {error}")))?;
    let mut nodes = Vec::new();
    collect_json_nodes(&body, &mut nodes, 0).map_err(|error| AppError::Integrity(error.into()))?;
    tracing::debug!(shape = %json_shape(&body), "received JSON upstream response shape");
    if let Some(error) = body.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .unwrap_or("upstream error")
            .to_owned();
        return Ok(vec![InternalEvent::Error { message }]);
    }
    if let Some(message) = json_error_message(&nodes) {
        return Ok(vec![InternalEvent::Error { message }]);
    }
    let mut events = Vec::new();
    let mut recognized = has_explicit_empty_response(&nodes);
    if let Some(text) = nodes
        .iter()
        .find_map(|node| json_text(node, &["content", "text", "output_text", "outputText"]))
    {
        if !text.is_empty() {
            events.push(InternalEvent::TextDelta { text });
        }
        recognized = true;
    }
    if let Some(text) =
        nodes.iter().find_map(|node| json_text(node, &["thinking", "reasoning", "reasoningText"]))
    {
        if !text.is_empty() {
            events.push(InternalEvent::ThinkingDelta { text });
        }
        recognized = true;
    }
    let mut seen_tool_calls = HashSet::new();
    for node in &nodes {
        for call in parse_json_tool_calls(node) {
            let dedupe_key = if call.id.is_empty() {
                format!("{}:{}", call.name, call.arguments_json())
            } else {
                call.id.clone()
            };
            if !seen_tool_calls.insert(dedupe_key) {
                continue;
            }
            recognized = true;
            let id = call.id.clone();
            let name = call.name.clone();
            let arguments = call.arguments_json();
            let complete = call.complete;
            events.push(InternalEvent::ToolCallStart { id: id.clone(), name });
            events.push(InternalEvent::ToolCallDelta { id: id.clone(), arguments, name: None });
            events.push(InternalEvent::ToolCallEnd { id, complete });
        }
    }
    if let Some(usage) = nodes.iter().find_map(|node| node.get("usage")) {
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
        recognized = true;
    }
    let reason = nodes
        .iter()
        .find_map(|node| {
            ["stopReason", "stop_reason", "finish_reason", "finishReason"]
                .iter()
                .find_map(|name| node.get(*name).and_then(Value::as_str))
        })
        .map(ToOwned::to_owned);
    if reason.is_some() {
        recognized = true;
    }
    if !recognized {
        return Err(AppError::Integrity(format!(
            "unrecognized JSON upstream response shape: {}",
            json_shape(&body)
        )));
    }
    events.push(InternalEvent::Stop { reason: reason.unwrap_or_else(|| "end_turn".into()) });
    Ok(events)
}

fn json_text(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        let value = value.get(*name)?;
        match value {
            Value::String(text) => Some(text.clone()),
            Value::Object(object) => object
                .get("text")
                .or_else(|| object.get("outputText"))
                .or_else(|| object.get("content"))
                .and_then(json_value_text),
            Value::Array(items) => {
                let text = items.iter().filter_map(json_value_text).collect::<Vec<_>>().join("");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        }
    })
}

fn json_value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => ["text", "output_text", "outputText", "content"]
            .iter()
            .find_map(|name| object.get(*name).and_then(json_value_text)),
        Value::Array(items) => {
            let text = items.iter().filter_map(json_value_text).collect::<Vec<_>>().join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn json_error_message(nodes: &[&Value]) -> Option<String> {
    nodes.iter().find_map(|node| {
        let object = node.as_object()?;
        let error_type = object.get("__type").and_then(Value::as_str)?;
        let message = object.get("message").and_then(Value::as_str)?;
        if message.is_empty() {
            return None;
        }
        Some(format!("{error_type}: {message}"))
    })
}

const MAX_JSON_DEPTH: usize = 8;
const MAX_JSON_NODES: usize = 512;

fn collect_json_nodes<'a>(
    value: &'a Value,
    nodes: &mut Vec<&'a Value>,
    depth: usize,
) -> Result<(), &'static str> {
    if depth > MAX_JSON_DEPTH {
        return Err("upstream JSON nesting exceeds configured depth");
    }
    if nodes.len() >= MAX_JSON_NODES {
        return Err("upstream JSON contains too many nodes");
    }
    nodes.push(value);
    match value {
        Value::Object(object) => {
            for child in object.values() {
                collect_json_nodes(child, nodes, depth + 1)?;
            }
        }
        Value::Array(items) => {
            for child in items.iter().take(64) {
                collect_json_nodes(child, nodes, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn has_explicit_empty_response(nodes: &[&Value]) -> bool {
    nodes.iter().any(|node| {
        let Some(object) = node.as_object() else {
            return false;
        };
        let empty_content =
            ["content", "output"].iter().filter_map(|name| object.get(*name)).any(|value| {
                match value {
                    Value::String(text) => text.is_empty(),
                    Value::Array(items) => items.is_empty(),
                    _ => false,
                }
            });
        let completed_status = object
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "complete" | "completed" | "succeeded"));
        empty_content || completed_status
    })
}

fn json_shape(value: &Value) -> String {
    fn render(value: &Value, depth: usize) -> String {
        if depth >= 3 {
            return match value {
                Value::Array(_) => "array".into(),
                Value::Object(_) => "object".into(),
                Value::String(_) => "string".into(),
                Value::Number(_) => "number".into(),
                Value::Bool(_) => "bool".into(),
                Value::Null => "null".into(),
            };
        }
        match value {
            Value::Object(object) => {
                let mut keys = object.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                let fields = keys
                    .into_iter()
                    .take(16)
                    .map(|key| format!("{key}:{}", render(&object[&key], depth + 1)))
                    .collect::<Vec<_>>();
                let suffix = (object.len() > 16).then_some(",...").unwrap_or_default();
                format!("object{{{}{suffix}}}", fields.join(","))
            }
            Value::Array(items) => {
                let shapes =
                    items.iter().take(4).map(|item| render(item, depth + 1)).collect::<Vec<_>>();
                let suffix = (items.len() > 4).then_some(",...").unwrap_or_default();
                format!("array[{}{suffix}]", shapes.join(","))
            }
            Value::String(_) => "string".into(),
            Value::Number(_) => "number".into(),
            Value::Bool(_) => "bool".into(),
            Value::Null => "null".into(),
        }
    }
    render(value, 0)
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
        let tail = self.xml_filter.finish();
        self.output.text.push_str(&tail);
        finish_stream_response(self.output, &mut self.tools)
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
                | "incomplete"
                | "stream_incomplete"
                | "upstream_disconnect"
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
    let mut items = Vec::new();
    for key in ["toolUses", "tool_uses", "toolUse", "toolCalls", "tool_calls", "toolCall", "output"]
    {
        match body.get(key) {
            Some(serde_json::Value::Array(values)) => items.extend(values.iter()),
            Some(value @ serde_json::Value::Object(_)) => items.push(value),
            _ => {}
        }
    }
    if items.is_empty()
        && body.get("name").is_some()
        && (body.get("input").is_some() || body.get("arguments").is_some())
    {
        items.push(body);
    }
    if items.is_empty() {
        return Vec::new();
    }
    items
        .iter()
        .filter_map(|item| {
            if item
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| !matches!(kind, "function_call" | "tool_use" | "function"))
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
fn is_empty_stream(response: &crate::protocol::internal::InternalResponse) -> bool {
    response.text.is_empty()
        && response.thinking.is_empty()
        && response.tool_calls.is_empty()
        && response.stop_reason.is_none()
}

#[cfg(test)]
mod tests {
    use super::{
        UpstreamClient, apply_internal_event, event_marks_completion, finish_stream_response,
        is_empty_stream, json_events, parse_json_tool_calls,
    };
    use crate::{
        auth::{AuthMethod, Credential, SecretString},
        endpoint::EndpointPolicy,
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
        let client = super::UpstreamClient::with_policy(
            reqwest::Client::new(),
            EndpointPolicy::Ide,
            Some(upstream_url),
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

    #[test]
    fn incomplete_terminal_reason_is_not_reported_as_completed() {
        let response = finish_stream_response(
            InternalResponse {
                text: "partial".into(),
                stop_reason: Some("stream_incomplete".into()),
                ..Default::default()
            },
            &mut ToolCallAccumulator::new(),
        );
        assert!(response.incomplete);
        assert!(!event_marks_completion(&InternalEvent::Stop {
            reason: "stream_incomplete".into(),
        }));
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let length = socket.read(&mut chunk).await.unwrap();
            assert!(length > 0, "request closed before headers were complete");
            bytes.extend_from_slice(&chunk[..length]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or_default();
        while bytes.len() < header_end + content_length {
            let length = socket.read(&mut chunk).await.unwrap();
            assert!(length > 0, "request closed before body was complete");
            bytes.extend_from_slice(&chunk[..length]);
        }
        String::from_utf8_lossy(&bytes[..header_end + content_length]).into_owned()
    }

    #[tokio::test]
    async fn auto_policy_uses_the_current_credential_endpoint_protocol() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (origin, model_id) in [("KIRO_CLI", "ide-model"), ("AI_EDITOR", "ide-model")] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                assert!(request.starts_with("POST /generateAssistantResponse "));
                assert!(request.contains(&format!("\"origin\":\"{origin}\"")));
                assert!(request.contains(&format!("\"modelId\":\"{model_id}\"")));
                assert_eq!(request.matches("x-amzn-codewhisperer-optout:").count(), 1);
                if origin == "KIRO_CLI" {
                    assert!(
                        request
                            .contains("x-amz-target: KiroRuntimeService.GenerateAssistantResponse")
                    );
                    assert!(request.contains("x-amzn-kiro-client-attribution: unrecognized"));
                    assert!(request.contains("x-kiro-attempt: 1;max=3"));
                    assert!(request.contains("x-amzn-codewhisperer-optout: false"));
                } else {
                    assert!(request.contains("x-amzn-codewhisperer-optout: true"));
                }
                let body = event_frame("assistantResponseEvent", json!({"content":"ok"}));
                let mut body = body;
                body.extend(event_frame("metadataEvent", json!({"stopReason":"end_turn"})));
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let client = UpstreamClient::with_policy(
            reqwest::Client::new(),
            EndpointPolicy::Auto,
            Some(format!("http://{address}/generateAssistantResponse")),
        );
        let request = InternalRequest {
            model: "ide-model".into(),
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
        for endpoint in ["cli", "ide"] {
            let credential = Credential {
                auth_method: AuthMethod::ApiKey,
                access_token: Some(SecretString::new("token")),
                endpoint: endpoint.into(),
                ..Default::default()
            };
            let mut events = client.event_stream(&request, &credential).await.unwrap();
            let mut collected = Vec::new();
            while let Some(event) = events.next().await {
                collected.push(event.unwrap());
            }
            assert!(matches!(
                &collected[..],
                [InternalEvent::TextDelta { text }, InternalEvent::Stop { reason }]
                    if text == "ok" && reason == "end_turn"
            ));
        }
        server.await.unwrap();
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
    fn adapts_nested_runtime_json_response() {
        let events = json_events(
            br#"{"assistantResponseEvent":{"content":[{"type":"output_text","text":"nested answer"}],"toolUses":[{"toolUseId":"call_1","name":"lookup","input":{"id":1}}],"usage":{"inputTokens":2,"outputTokens":3},"stopReason":"end_turn"}}"#,
        )
        .unwrap();
        assert!(matches!(
            &events[0],
            InternalEvent::TextDelta { text } if text == "nested answer"
        ));
        assert!(matches!(
            &events[1],
            InternalEvent::ToolCallStart { id, name } if id == "call_1" && name == "lookup"
        ));
        assert!(
            matches!(&events[2], InternalEvent::ToolCallDelta { arguments, .. } if arguments == r#"{"id":1}"#)
        );
        assert!(matches!(&events[3], InternalEvent::ToolCallEnd { complete: true, .. }));
        assert!(
            matches!(&events[4], InternalEvent::Usage { usage } if usage.input_tokens == 2 && usage.output_tokens == 3)
        );
        assert!(matches!(
            &events[5],
            InternalEvent::Stop { reason } if reason == "end_turn"
        ));
    }

    #[test]
    fn adapts_nested_message_content_json_response() {
        let events = json_events(
            br#"{"response":{"message":{"content":[{"type":"text","text":"deep answer"}]}}}"#,
        )
        .unwrap();
        assert!(matches!(
            &events[..],
            [InternalEvent::TextDelta { text }, InternalEvent::Stop { reason }]
                if text == "deep answer" && reason == "end_turn"
        ));
    }

    #[test]
    fn rejects_unrecognized_success_json_instead_of_emitting_empty_stop() {
        let error = json_events(br#"{"status":"ok","metadata":{"requestId":"redacted"}}"#)
            .expect_err("unknown successful JSON must not become an empty response");
        assert!(matches!(
            error,
            crate::error::AppError::Integrity(message)
                if message.contains("unrecognized JSON upstream response shape")
        ));
    }

    #[test]
    fn rejects_json_that_exceeds_node_depth_budget() {
        let mut value = serde_json::json!({"content":"ok"});
        for _ in 0..12 {
            value = serde_json::json!({"nested": value});
        }
        let body = serde_json::to_vec(&value).unwrap();
        let error = json_events(&body).expect_err("deep upstream JSON must be rejected");
        assert!(error.to_string().contains("nesting"));
    }

    #[test]
    fn rejects_generic_message_json_instead_of_emitting_empty_stop() {
        let error = json_events(br#"{"message":"completed","metadata":{"requestId":"redacted"}}"#)
            .expect_err("generic message JSON must not become an empty response");
        assert!(matches!(error, crate::error::AppError::Integrity(_)));
    }

    #[test]
    fn preserves_explicit_empty_runtime_response() {
        let events =
            json_events(br#"{"assistantResponseEvent":{"content":"","stopReason":"end_turn"}}"#)
                .unwrap();
        assert!(matches!(
            &events[..],
            [InternalEvent::Stop { reason }] if reason == "end_turn"
        ));
    }

    #[test]
    fn preserves_explicit_empty_completed_response() {
        let events = json_events(br#"{"response":{"status":"completed","output":[]}}"#).unwrap();
        assert!(matches!(
            &events[..],
            [InternalEvent::Stop { reason }] if reason == "end_turn"
        ));
    }

    #[test]
    fn adapts_json_upstream_error_to_error_event() {
        let events = json_events(br#"{"error":{"message":"overloaded"}}"#).unwrap();
        assert!(
            matches!(&events[..], [InternalEvent::Error { message }] if message == "overloaded")
        );
    }

    #[test]
    fn adapts_cli_json_error_envelope_to_error_event() {
        let events = json_events(
            br#"{"Output":{"__type":"ModelError","message":"request rejected"},"Version":"1.0"}"#,
        )
        .unwrap();
        assert!(matches!(
            &events[..],
            [InternalEvent::Error { message }] if message == "ModelError: request rejected"
        ));
    }

    #[test]
    fn adapts_nested_output_text_and_openai_function_calls() {
        let events = json_events(
            br#"{"output":[{"type":"message","content":[{"type":"output_text","text":"nested"}]},{"type":"function","id":"call_1","function":{"name":"lookup","arguments":"{\"x\":1}"}}]}"#,
        )
        .unwrap();
        assert!(matches!(&events[0], InternalEvent::TextDelta { text } if text == "nested"));
        assert!(
            matches!(&events[1], InternalEvent::ToolCallStart { id, name } if id == "call_1" && name == "lookup")
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
    fn completion_logging_tracks_terminal_internal_events() {
        assert!(event_marks_completion(&InternalEvent::Stop { reason: "end_turn".into() }));
        assert!(event_marks_completion(&InternalEvent::ToolCallEnd {
            id: "call_1".into(),
            complete: true,
        }));
        assert!(!event_marks_completion(&InternalEvent::ToolCallEnd {
            id: "call_1".into(),
            complete: false,
        }));
        assert!(!event_marks_completion(&InternalEvent::TextDelta { text: "answer".into() }));
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
