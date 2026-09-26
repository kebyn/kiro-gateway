use crate::{
    auth::Credential,
    endpoint::{EndpointPolicy, endpoint_for},
    error::AppError,
    generation::{GenerationEvent, GenerationRequest},
    upstream::{
        error::UpstreamStreamError,
        event_stream::{EventStreamDecoder, decode_generation_events},
        integrity::{RetryDecision, StreamIntegrity},
        json::decode_events,
        transport::{SendError, send_once},
    },
};
use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::Client;
use std::pin::Pin;
use std::time::Instant;

pub type GenerationEventStream =
    Pin<Box<dyn Stream<Item = Result<GenerationEvent, AppError>> + Send>>;

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
        request: &GenerationRequest,
        credential: &Credential,
    ) -> Result<GenerationEventStream, AppError> {
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
                let response = match send_once(
                    &client,
                    &endpoint,
                    &request,
                    &credential,
                    max_body_bytes,
                )
                .await
                {
                    Ok(response) => response,
                    Err(SendError::Transport(error)) if attempt == 0 => {
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
                    match decode_events(&body) {
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
                        let events = match decode_generation_events(&message) {
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
                    yield Ok(GenerationEvent::Stop { reason: "stream_incomplete".into() });
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

fn event_marks_completion(event: &GenerationEvent) -> bool {
    matches!(
        event,
        GenerationEvent::Stop { reason } if !reason.is_incomplete()
    ) || matches!(event, GenerationEvent::ToolCallEnd { complete: true, .. })
}

#[cfg(test)]
fn is_empty_stream(response: &crate::generation::GenerationResult) -> bool {
    response.text.is_empty()
        && response.thinking.is_empty()
        && response.tool_calls.is_empty()
        && response.stop_reason.is_none()
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
