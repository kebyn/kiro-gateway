#[cfg(test)]
use crate::protocol::internal::InternalResponse;
use crate::{
    AppState,
    error::{AppError, Protocol, protocol_error_response},
    protocol::{
        internal::{InternalEvent, InternalRequest},
        openai_chat::ChatRequest,
    },
    transform::converter::{chat_finish_reason, openai_chat_response},
    transform::truncation::XmlLeakFilter,
    upstream::request::InternalEventAccumulator,
};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::StreamExt;
use serde_json::json;
use std::{collections::HashMap, convert::Infallible};

pub async fn route(
    State(state): State<AppState>,
    body: Result<Json<ChatRequest>, JsonRejection>,
) -> Response {
    match body {
        Ok(body) => match chat_completions(State(state), body).await {
            Ok(response) => response,
            Err(error) => protocol_error_response(Protocol::ChatCompletions, error),
        },
        Err(rejection) => {
            let message = rejection.to_string();
            let error = if message.to_ascii_lowercase().contains("limit")
                || message.to_ascii_lowercase().contains("too large")
            {
                AppError::PayloadTooLarge
            } else {
                AppError::BadRequest(format!("invalid JSON request: {message}"))
            };
            protocol_error_response(Protocol::ChatCompletions, error)
        }
    }
}

pub async fn chat_completions(
    State(state): State<AppState>,
    Json(body): Json<ChatRequest>,
) -> Result<Response, AppError> {
    body.validate().map_err(AppError::BadRequest)?;
    let mut request: InternalRequest = body.into();
    request.model = state.token_manager.resolve_model(&request.model).await?;
    tracing::debug!(
        protocol = "openai_chat",
        model = %request.model,
        stream = request.stream,
        messages = request.messages.len(),
        tools = request.tools.len(),
        "accepted OpenAI Chat Completions request"
    );
    let stream_response = request.stream;
    let model = request.model.clone();
    if !stream_response {
        let response = state.complete(&request).await?;
        return Ok(Json(openai_chat_response(&model, &response)).into_response());
    }
    let upstream = state.event_stream(&request).await?;
    let id = format!("chatcmpl-{}", uuid::Uuid::now_v7());
    let created = chrono::Utc::now().timestamp();
    let stream = async_stream::stream! {
        let mut upstream = upstream;
        let first = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]});
        yield Ok::<Event, Infallible>(Event::default().data(first.to_string()));
        let mut accumulator = InternalEventAccumulator::new();
        let mut tool_indices = HashMap::<String, usize>::new();
        let mut next_tool_index = 0_usize;
        let mut text_filter = XmlLeakFilter::new();
        let mut failed = false;
        let chunk = |delta: serde_json::Value| {
            json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
        };
        while let Some(item) = upstream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(error) => {
                    yield Ok(Event::default().event("error").data(json!({"error":{"message":error.to_string(),"type":"upstream_error"}}).to_string()));
                    yield Ok(Event::default().data("[DONE]"));
                    failed = true;
                    break;
                }
            };
            if let InternalEvent::Error { message } = &event {
                yield Ok(Event::default().event("error").data(json!({"error":{"message":message,"type":"upstream_error"}}).to_string()));
                yield Ok(Event::default().data("[DONE]"));
                failed = true;
                break;
            }
            if let Err(error) = accumulator.push(event.clone()) {
                yield Ok(Event::default().event("error").data(json!({"error":{"message":error.to_string(),"type":"upstream_error"}}).to_string()));
                yield Ok(Event::default().data("[DONE]"));
                failed = true;
                break;
            }
            match event {
                InternalEvent::TextDelta { text } if !text.is_empty() => {
                    let text = text_filter.push(&text);
                    if !text.is_empty() {
                        yield Ok(Event::default().data(chunk(json!({"content":text})).to_string()));
                    }
                }
                InternalEvent::ThinkingDelta { text } if !text.is_empty() => {
                    yield Ok(Event::default().data(chunk(json!({"reasoning_content":text})).to_string()));
                }
                InternalEvent::ToolCallStart { id: call_id, name } => {
                    let index = *tool_indices.entry(call_id.clone()).or_insert_with(|| {
                        let value = next_tool_index;
                        next_tool_index += 1;
                        value
                    });
                    yield Ok(Event::default().data(chunk(json!({"tool_calls":[{"index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":""}}]})).to_string()));
                }
                InternalEvent::ToolCallDelta { id: call_id, arguments, name } => {
                    let index = *tool_indices.entry(call_id.clone()).or_insert_with(|| {
                        let value = next_tool_index;
                        next_tool_index += 1;
                        value
                    });
                    let mut function = json!({"arguments":arguments});
                    if let Some(name) = name { function["name"] = json!(name); }
                    yield Ok(Event::default().data(chunk(json!({"tool_calls":[{"index":index,"function":function}]})).to_string()));
                }
                InternalEvent::ToolCallEnd { .. } | InternalEvent::Usage { .. } | InternalEvent::Stop { .. } => {}
                InternalEvent::Error { .. } | InternalEvent::TextDelta { .. } | InternalEvent::ThinkingDelta { .. } => {}
            }
        }
        if failed { return; }
        let tail = text_filter.finish();
        if !tail.is_empty() {
            yield Ok(Event::default().data(chunk(json!({"content":tail})).to_string()));
        }
        let response = accumulator.finish();
        tracing::debug!(
            protocol = "openai_chat",
            model = %model,
            stream_end_status = if response.incomplete { "incomplete" } else { "completed" },
            "OpenAI Chat stream completed"
        );
        let finish = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":chat_finish_reason(&response)}]});
        yield Ok(Event::default().data(finish.to_string()));
        yield Ok(Event::default().data("[DONE]"));
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
}

#[cfg(test)]
fn chat_stream_data(payload: &serde_json::Value, response: &InternalResponse) -> Vec<String> {
    let chunk = |delta: serde_json::Value, finish_reason: serde_json::Value| {
        json!({
            "id":payload["id"],
            "object":"chat.completion.chunk",
            "created":payload["created"],
            "model":payload["model"],
            "choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}]
        })
        .to_string()
    };
    let mut role_delta = json!({"role":"assistant"});
    if !response.text.is_empty() {
        role_delta["content"] = serde_json::Value::String(response.text.clone());
    }
    let mut chunks = vec![chunk(role_delta, serde_json::Value::Null)];
    for (index, call) in response.tool_calls.iter().enumerate() {
        chunks.push(chunk(
            json!({
                "tool_calls":[{
                    "index":index,
                    "id":call.id,
                    "type":"function",
                    "function":{"name":call.name,"arguments":call.arguments_json()}
                }]
            }),
            serde_json::Value::Null,
        ));
    }
    chunks.push(chunk(json!({}), json!(chat_finish_reason(response))));
    chunks.push("[DONE]".into());
    chunks
}

#[cfg(test)]
mod tests {
    use super::{chat_completions, chat_stream_data};
    use crate::{
        AppState,
        app_state::build_upstream,
        auth::{AuthMethod, Credential},
        config::AppConfig,
        credential::TokenManager,
        error::AppError,
        model_catalog::ModelInfo,
        protocol::internal::{InternalResponse, InternalToolCall},
        response_store::ResponseStore,
        transform::converter::openai_chat_response,
    };
    use axum::{Json, extract::State};
    use serde_json::{Value, json};
    use std::{path::Path, sync::Arc};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn state(path: &Path) -> AppState {
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let token_manager = Arc::new(TokenManager::new(&config, credential).unwrap());
        token_manager.seed_models_for_tests(vec![ModelInfo {
            model_id: "kiro".into(),
            model_name: None,
            description: None,
            token_limits: None,
        }]);
        AppState {
            config: Arc::new(config.clone()),
            token_manager,
            responses: ResponseStore::open(path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        }
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) {
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
    }

    async fn serve_json(listener: TcpListener, body: Vec<u8>) {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_http_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(&body).await.unwrap();
    }

    async fn state_with_json_upstream(
        path: &Path,
        body: Vec<u8>,
    ) -> (AppState, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_json(listener, body));
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            response_store_path: path.display().to_string(),
            upstream_url: Some(format!("http://{address}")),
            ..Default::default()
        };
        let credential = Credential {
            auth_method: AuthMethod::ApiKey,
            access_token: Some(crate::auth::SecretString::new("token")),
            ..Default::default()
        };
        let token_manager = Arc::new(TokenManager::new(&config, credential).unwrap());
        token_manager.seed_models_for_tests(vec![ModelInfo {
            model_id: "kiro".into(),
            model_name: None,
            description: None,
            token_limits: None,
        }]);
        (
            AppState {
                config: Arc::new(config.clone()),
                token_manager,
                responses: ResponseStore::open(path).unwrap(),
                upstream: build_upstream(&config, reqwest::Client::new()),
                sessions: Default::default(),
            },
            server,
        )
    }

    #[tokio::test]
    async fn live_stream_rejects_missing_upstream_credential() {
        let directory = tempfile::tempdir().unwrap();
        let request = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"user","content":"hello"}],
            "stream":true
        }))
        .unwrap();
        let error = chat_completions(
            State(state(&directory.path().join("responses.sqlite3"))),
            Json(request),
        )
        .await
        .expect_err("missing upstream credentials must not produce a successful response");
        assert!(matches!(error, AppError::Credential(message) if message.contains("access token")));
    }

    #[tokio::test]
    async fn live_stream_emits_openai_lifecycle_and_done_marker() {
        let directory = tempfile::tempdir().unwrap();
        let (state, server) = state_with_json_upstream(
            &directory.path().join("responses.sqlite3"),
            br#"{"content":"hello","stopReason":"end_turn"}"#.to_vec(),
        )
        .await;
        let request = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"user","content":"hello"}],
            "stream":true
        }))
        .unwrap();
        let response = chat_completions(State(state), Json(request)).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("\"role\":\"assistant\""));
        assert!(body.contains("\"content\":\"hello\""));
        assert!(body.contains("\"finish_reason\":\"stop\""));
        assert!(body.contains("data: [DONE]\n"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_stream_returns_openai_chat_response() {
        let directory = tempfile::tempdir().unwrap();
        let (state, server) = state_with_json_upstream(
            &directory.path().join("responses.sqlite3"),
            br#"{"content":"hello","stopReason":"end_turn"}"#.to_vec(),
        )
        .await;
        let request = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false
        }))
        .unwrap();
        let response = chat_completions(State(state), Json(request)).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_stream_emits_openai_tool_calls() {
        let directory = tempfile::tempdir().unwrap();
        let (state, server) = state_with_json_upstream(
            &directory.path().join("responses.sqlite3"),
            br#"{"toolCalls":[{"id":"call_1","name":"bash","arguments":{"command":"pwd"}}],"stopReason":"tool_use"}"#.to_vec(),
        )
        .await;
        let request = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"user","content":"run pwd"}],
            "tools":[{"type":"function","function":{"name":"bash","parameters":{"type":"object"}}}],
            "stream":true
        }))
        .unwrap();
        let response = chat_completions(State(state), Json(request)).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("\"type\":\"function\""));
        assert!(body.contains("\"name\":\"bash\""));
        assert!(body.contains("\"arguments\":\"{\\\"command\\\":\\\"pwd\\\"}\""));
        assert!(body.contains("\"finish_reason\":\"tool_calls\""));
        assert!(body.contains("data: [DONE]\n"));
        server.await.unwrap();
    }

    #[test]
    fn streams_parallel_tool_calls_before_done() {
        let response = InternalResponse {
            tool_calls: vec![
                InternalToolCall {
                    id: "call_a".into(),
                    name: "alpha".into(),
                    arguments: json!({"a":1}),
                    complete: true,
                },
                InternalToolCall {
                    id: "call_b".into(),
                    name: "beta".into(),
                    arguments: json!({"b":2}),
                    complete: true,
                },
            ],
            ..Default::default()
        };
        let payload = openai_chat_response("kiro", &response);
        let data = chat_stream_data(&payload, &response);
        let chunks: Vec<Value> =
            data[..data.len() - 1].iter().map(|item| serde_json::from_str(item).unwrap()).collect();
        assert!(chunks[0]["choices"][0]["delta"].get("content").is_none());
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            r#"{"a":1}"#
        );
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
        assert_eq!(chunks[3]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(data.last().unwrap(), "[DONE]");
    }
}
