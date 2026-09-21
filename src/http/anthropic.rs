#[cfg(test)]
use crate::protocol::internal::InternalResponse;
use crate::transform::truncation::XmlLeakFilter;
use crate::{
    AppState,
    error::{AppError, Protocol, protocol_error_response},
    protocol::{
        anthropic::{CountTokensRequest, MessagesRequest},
        internal::{InternalEvent, InternalRequest},
    },
    transform::converter::{anthropic_response, anthropic_stop_reason},
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
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
};

/// Router-facing wrapper that keeps JSON parsing failures in the Anthropic
/// error contract. The underlying handler remains directly callable in unit
/// tests and returns `AppError` for internal composition.
pub async fn route(
    State(state): State<AppState>,
    body: Result<Json<MessagesRequest>, JsonRejection>,
) -> Response {
    match body {
        Ok(body) => match messages(State(state), body).await {
            Ok(response) => response,
            Err(error) => protocol_error_response(Protocol::Anthropic, error),
        },
        Err(rejection) => {
            protocol_error_response(Protocol::Anthropic, json_rejection_error(rejection))
        }
    }
}

pub async fn count_tokens_route(
    State(state): State<AppState>,
    body: Result<Json<CountTokensRequest>, JsonRejection>,
) -> Response {
    match body {
        Ok(body) => match count_tokens(State(state), body).await {
            Ok(response) => response.into_response(),
            Err(error) => protocol_error_response(Protocol::Anthropic, error),
        },
        Err(rejection) => {
            protocol_error_response(Protocol::Anthropic, json_rejection_error(rejection))
        }
    }
}

fn json_rejection_error(rejection: JsonRejection) -> AppError {
    let message = rejection.to_string();
    if message.to_ascii_lowercase().contains("body limit")
        || message.to_ascii_lowercase().contains("length limit")
        || message.to_ascii_lowercase().contains("too large")
    {
        AppError::PayloadTooLarge
    } else {
        AppError::BadRequest(format!("invalid JSON request: {message}"))
    }
}

pub async fn messages(
    State(state): State<AppState>,
    Json(body): Json<MessagesRequest>,
) -> Result<Response, AppError> {
    let mut request: InternalRequest = body.into();
    request.model = state.token_manager.resolve_model(&request.model).await?;
    tracing::debug!(
        protocol = "anthropic",
        model = %request.model,
        stream = request.stream,
        messages = request.messages.len(),
        tools = request.tools.len(),
        "accepted Anthropic Messages request"
    );
    let stream_response = request.stream;
    let model = request.model.clone();
    if !stream_response {
        let response = state.complete(&request).await?;
        return Ok(Json(anthropic_response(&model, &response)).into_response());
    }
    let upstream = state.event_stream(&request).await?;
    let message_id = format!("msg_{}", uuid::Uuid::now_v7());
    let input_tokens = request.input_text().chars().count() as u64 / 4;
    let stream = async_stream::stream! {
        tracing::debug!(model = %model, "starting Anthropic SSE response");
        let start = json!({
            "type":"message_start",
            "message": {
                "id":message_id,
                "type":"message",
                "role":"assistant",
                "model":model,
                "content":[],
                "stop_reason":null,
                "stop_sequence":null,
                "usage":{"input_tokens":input_tokens,"output_tokens":0}
            }
        });
        yield Ok::<Event, Infallible>(Event::default().event("message_start").data(start.to_string()));

        let mut upstream = upstream;
        let mut accumulator = InternalEventAccumulator::new();
        let mut text_index = None;
        let mut thinking_index = None;
        let mut tool_indices = HashMap::<String, usize>::new();
        let mut active_tool = None::<String>;
        let mut block_order = Vec::<usize>::new();
        let mut closed_blocks = HashSet::<usize>::new();
        let mut next_index = 0_usize;
        let mut text_filter = XmlLeakFilter::new();
        let mut failed = false;

        while let Some(item) = upstream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(error) => {
                    tracing::warn!(model = %model, error_class = "upstream", "Anthropic upstream stream failed");
                    yield Ok(Event::default().event("error").data(json!({"type":"error","error":{"type":"upstream_error","message":error.to_string()}}).to_string()));
                    yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
                    failed = true;
                    break;
                }
            };
            if let InternalEvent::Error { message } = &event {
                tracing::warn!(model = %model, error_class = "upstream", "Anthropic upstream returned an error event");
                yield Ok(Event::default().event("error").data(json!({"type":"error","error":{"type":"upstream_error","message":message}}).to_string()));
                yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
                failed = true;
                break;
            }
            if let Err(error) = accumulator.push(event.clone()) {
                yield Ok(Event::default().event("error").data(json!({"type":"error","error":{"type":"upstream_error","message":error.to_string()}}).to_string()));
                yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
                failed = true;
                break;
            }
            match event {
                InternalEvent::TextDelta { text } => {
                    if let Some(index) = thinking_index {
                        if closed_blocks.insert(index) {
                            yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
                        }
                        thinking_index = None;
                    }
                    let was_none = text_index.is_none();
                    let index = *text_index.get_or_insert_with(|| {
                        let value = next_index;
                        next_index += 1;
                        value
                    });
                    if was_none {
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}).to_string()));
                    }
                    let text = text_filter.push(&text);
                    if !text.is_empty() {
                        yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}).to_string()));
                    }
                }
                InternalEvent::ThinkingDelta { text } => {
                    if let Some(index) = text_index {
                        if closed_blocks.insert(index) {
                            yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
                        }
                        text_index = None;
                    }
                    let was_none = thinking_index.is_none();
                    let index = *thinking_index.get_or_insert_with(|| {
                        let value = next_index;
                        next_index += 1;
                        value
                    });
                    if was_none {
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":""}}).to_string()));
                    }
                    if !text.is_empty() {
                        yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":text}}).to_string()));
                    }
                }
                InternalEvent::ToolCallStart { id, name } => {
                    for index in [text_index, thinking_index].into_iter().flatten() {
                        if closed_blocks.insert(index) {
                            yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
                        }
                    }
                    text_index = None;
                    thinking_index = None;
                    let key = if id.is_empty() {
                        active_tool.clone().unwrap_or_else(|| format!("tool_call_{}", tool_indices.len() + 1))
                    } else { id };
                    if !tool_indices.contains_key(&key) {
                        let index = next_index;
                        next_index += 1;
                        tool_indices.insert(key.clone(), index);
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":key,"name":name,"input":{}}}).to_string()));
                    }
                    active_tool = Some(key);
                }
                InternalEvent::ToolCallDelta { id, arguments, name } => {
                    for index in [text_index, thinking_index].into_iter().flatten() {
                        if closed_blocks.insert(index) {
                            yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
                        }
                    }
                    text_index = None;
                    thinking_index = None;
                    let mut key = id;
                    if key.is_empty() {
                        key = active_tool.clone().unwrap_or_else(|| format!("tool_call_{}", tool_indices.len() + 1));
                    }
                    if !tool_indices.contains_key(&key) {
                        let index = next_index;
                        next_index += 1;
                        tool_indices.insert(key.clone(), index);
                        block_order.push(index);
                        yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":key,"name":name.unwrap_or_default(),"input":{}}}).to_string()));
                    }
                    active_tool = Some(key.clone());
                    if !arguments.is_empty() {
                        yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":tool_indices[&key],"delta":{"type":"input_json_delta","partial_json":arguments}}).to_string()));
                    }
                }
                InternalEvent::ToolCallEnd { id, .. } => {
                    let key = if id.is_empty() { active_tool.clone() } else { Some(id) };
                    if let Some(key) = key {
                        if let Some(index) = tool_indices.get(&key).copied() {
                            if closed_blocks.insert(index) {
                                yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
                            }
                        }
                        if active_tool.as_deref() == Some(key.as_str()) { active_tool = None; }
                    }
                }
                InternalEvent::Usage { .. } | InternalEvent::Stop { .. } => {}
                InternalEvent::Error { .. } => unreachable!(),
            }
        }
        if failed { return; }
        let tail = text_filter.finish();
        if !tail.is_empty() {
            if let Some(index) = thinking_index {
                if closed_blocks.insert(index) {
                    yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
                }
            }
            let was_none = text_index.is_none();
            let index = *text_index.get_or_insert_with(|| {
                let value = next_index;
                next_index += 1;
                value
            });
            if was_none {
                block_order.push(index);
                yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}).to_string()));
            }
            yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":tail}}).to_string()));
        }
        let response = accumulator.finish();
        tracing::debug!(
            protocol = "anthropic",
            model = %model,
            text_chars = response.text.chars().count(),
            thinking_chars = response.thinking.chars().count(),
            tool_calls = response.tool_calls.len(),
            stop_reason = ?response.stop_reason,
            stream_end_status = if response.incomplete { "incomplete" } else { "completed" },
            "completed Anthropic SSE response"
        );
        for index in block_order {
            if !closed_blocks.contains(&index) {
                yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":index}).to_string()));
            }
        }
        let output_tokens = response.usage.as_ref().map_or(0, |usage| usage.output_tokens);
        yield Ok(Event::default().event("message_delta").data(json!({"type":"message_delta","delta":{"stop_reason":anthropic_stop_reason(&response),"stop_sequence":null},"usage":{"output_tokens":output_tokens}}).to_string()));
        yield Ok(Event::default().event("message_stop").data(json!({"type":"message_stop"}).to_string()));
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
}

#[cfg(test)]
fn anthropic_stream_events(
    payload: &serde_json::Value,
    response: &InternalResponse,
) -> Vec<(&'static str, serde_json::Value)> {
    let mut events = vec![(
        "message_start",
        json!({
            "type":"message_start",
            "message": {
                "id": payload["id"],
                "type": "message",
                "role": "assistant",
                "model": payload["model"],
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": payload["usage"]["input_tokens"],
                    "output_tokens": 0,
                }
            }
        }),
    )];
    let mut index = 0;
    if !response.thinking.is_empty() {
        events.push((
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":""}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":response.thinking}}),
        ));
        events.push(("content_block_stop", json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    if !response.text.is_empty() {
        events.push((
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":response.text}}),
        ));
        events.push(("content_block_stop", json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    for call in &response.tool_calls {
        events.push((
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":call.id,"name":call.name,"input":{}}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":call.arguments_json()}}),
        ));
        events.push(("content_block_stop", json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    events.push((
        "message_delta",
        json!({
            "type":"message_delta",
            "delta":{"stop_reason":anthropic_stop_reason(response),"stop_sequence":null},
            "usage":{"output_tokens":payload["usage"]["output_tokens"]}
        }),
    ));
    events.push(("message_stop", json!({"type":"message_stop"})));
    events
}
pub async fn count_tokens(
    State(state): State<AppState>,
    Json(body): Json<CountTokensRequest>,
) -> Result<impl IntoResponse, AppError> {
    state.token_manager.resolve_model(&body._model).await?;
    let text = body
        .messages
        .iter()
        .map(|message| match &message.content {
            serde_json::Value::String(v) => v.clone(),
            v => v.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let tools = serde_json::to_string(&body.tools).unwrap_or_default();
    Ok(Json(json!({"input_tokens": (text.chars().count() + tools.chars().count()) as u64 / 4 + 1})))
}

#[cfg(test)]
mod tests {
    use super::{anthropic_stream_events, messages};
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
        transform::converter::anthropic_response,
    };
    use axum::{Json, extract::State};
    use serde_json::json;
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

    async fn stream_state(path: &Path, upstream_url: String) -> AppState {
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            upstream_url: Some(upstream_url),
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
        AppState {
            config: Arc::new(config.clone()),
            token_manager,
            responses: ResponseStore::open(path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        }
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
        let error =
            messages(State(state(&directory.path().join("responses.sqlite3"))), Json(request))
                .await
                .expect_err("missing upstream credentials must not produce a successful response");
        assert!(matches!(error, AppError::Credential(message) if message.contains("access token")));
    }

    #[tokio::test]
    async fn live_stream_emits_anthropic_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_json(
            listener,
            br#"{"content":"hello","stopReason":"end_turn"}"#.to_vec(),
        ));

        let state = stream_state(
            &directory.path().join("responses.sqlite3"),
            format!("http://{address}/generateAssistantResponse"),
        )
        .await;
        let request = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"user","content":"hello"}],
            "stream":true
        }))
        .unwrap();
        let response = messages(State(state), Json(request)).await.unwrap();
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
        assert!(body.contains("event: message_start\n"));
        assert!(body.contains("event: content_block_start\n"));
        assert!(body.contains("hello"));
        assert!(body.contains("event: message_delta\n"));
        assert!(body.contains("event: message_stop\n"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_stream_emits_tool_use_blocks_for_anthropic_clients() {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_json(
            listener,
            br#"{"toolUses":[{"toolUseId":"call_1","name":"bash","input":{"command":"pwd"}}],"stopReason":"tool_use"}"#.to_vec(),
        ));

        let state = stream_state(
            &directory.path().join("responses.sqlite3"),
            format!("http://{address}/generateAssistantResponse"),
        )
        .await;
        let request = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"user","content":"run pwd"}],
            "tools":[{"name":"bash","input_schema":{"type":"object"}}],
            "stream":true
        }))
        .unwrap();
        let response = messages(State(state), Json(request)).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("\"type\":\"tool_use\""));
        assert!(body.contains("\"name\":\"bash\""));
        assert!(body.contains("\"partial_json\":\"{\\\"command\\\":\\\"pwd\\\"}\""));
        assert!(body.contains("\"stop_reason\":\"tool_use\""));
        assert!(body.contains("event: message_stop\n"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_stream_messages_returns_anthropic_json_response() {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_json(
            listener,
            br#"{"content":"hello","stopReason":"end_turn"}"#.to_vec(),
        ));

        let state = stream_state(
            &directory.path().join("responses.sqlite3"),
            format!("http://{address}/generateAssistantResponse"),
        )
        .await;
        let request = serde_json::from_value(serde_json::json!({
            "model":"kiro",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false
        }))
        .unwrap();
        let response = messages(State(state), Json(request)).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["content"][0]["text"], "hello");
        assert_eq!(body["stop_reason"], "end_turn");
        server.await.unwrap();
    }

    #[test]
    fn streams_tool_use_blocks_with_protocol_indices() {
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
        let payload = anthropic_response("kiro", &response);
        let events = anthropic_stream_events(&payload, &response);
        assert_eq!(
            events.iter().map(|event| event.0).collect::<Vec<_>>(),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[2].1["delta"]["partial_json"], r#"{"a":1}"#);
        assert_eq!(events[4].1["index"], 1);
        assert_eq!(events[7].1["delta"]["stop_reason"], "tool_use");
    }
}
