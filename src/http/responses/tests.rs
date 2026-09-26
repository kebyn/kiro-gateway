use super::{
    create, response_messages, responses_live_stream, responses_payload,
    responses_payload_with_tools, responses_stream_events,
};
use crate::{
    AppState,
    app_state::build_upstream,
    auth::{AuthMethod, Credential, SecretString},
    config::AppConfig,
    credential::TokenManager,
    generation::{GenerationResult, Message, ToolCall, ToolDefinition},
    model_catalog::ModelInfo,
    protocol::openai_responses::ResponsesRequest,
    response_store::{ResponseStatus, ResponseStore},
};
use axum::response::IntoResponse;
use axum::{Json, extract::State};
use futures_util::stream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn response(text: &str) -> GenerationResult {
    GenerationResult {
        text: text.into(),
        tool_calls: vec![ToolCall {
            id: "call_upstream".into(),
            name: "lookup".into(),
            arguments: json!({"id":42}),
            complete: true,
        }],
        ..Default::default()
    }
}

fn state(path: &Path) -> AppState {
    let config = AppConfig {
        client_api_key: "client".into(),
        admin_api_key: "admin".into(),
        response_store_path: path.display().to_string(),
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
    let client = reqwest::Client::new();
    AppState {
        config: Arc::new(config.clone()),
        token_manager,
        responses: ResponseStore::open(path).unwrap(),
        upstream: build_upstream(&config, client),
        sessions: Default::default(),
    }
}

async fn state_with_json_upstream(path: &Path) -> (AppState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        let body = br#"{"content":"answer","usage":{"inputTokens":1,"outputTokens":1},"stopReason":"end_turn"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
    });
    let config = AppConfig {
        client_api_key: "client".into(),
        admin_api_key: "admin".into(),
        response_store_path: path.display().to_string(),
        upstream_url: Some(format!("http://{address}")),
        ..Default::default()
    };
    let credential = Credential {
        auth_method: AuthMethod::ApiKey,
        access_token: Some(SecretString::new("token")),
        ..Default::default()
    };
    let token_manager = Arc::new(TokenManager::new(&config, credential).unwrap());
    token_manager.seed_models_for_tests(vec![ModelInfo {
        model_id: "kiro".into(),
        model_name: None,
        description: None,
        token_limits: None,
    }]);
    let client = reqwest::Client::new();
    (
        AppState {
            config: Arc::new(config.clone()),
            token_manager,
            responses: ResponseStore::open(path).unwrap(),
            upstream: build_upstream(&config, client),
            sessions: Default::default(),
        },
        server,
    )
}

#[tokio::test]
async fn live_stream_emits_incremental_events_and_store_false_leaves_no_record() {
    let directory = tempfile::tempdir().unwrap();
    let (state, server) =
        state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
    let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"kiro",
            "input":[
                {"type":"reasoning","id":"rs_stream","summary":[],"encrypted_content":"opaque-reasoning"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}
            ],
            "store":false,
            "stream":true
        }))
        .unwrap();
    let response = create(State(state.clone()), Json(request)).await.unwrap();
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    server.await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("response.created"));
    assert!(body.contains("response.output_text.delta"));
    assert!(body.contains("response.completed"));
    let id = body
        .lines()
        .find(|line| line.starts_with("data: ") && line.contains("response.created"))
        .and_then(|line| serde_json::from_str::<Value>(line.trim_start_matches("data: ")).ok())
        .and_then(|value| value["response"]["id"].as_str().map(ToOwned::to_owned))
        .unwrap();
    assert!(state.responses.get(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn non_stream_returns_openai_response_payload() {
    let directory = tempfile::tempdir().unwrap();
    let (state, server) =
        state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"kiro",
        "input":"hello",
        "store":false,
        "stream":false
    }))
    .unwrap();
    let response = create(State(state), Json(request)).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    server.await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output_text"], "answer");
}

#[tokio::test]
async fn non_stream_accepts_codex_reasoning_history() {
    let directory = tempfile::tempdir().unwrap();
    let (state, server) =
        state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"kiro",
        "input":[
            {
                "type":"reasoning",
                "id":"rs_1",
                "summary":[],
                "encrypted_content":"opaque-reasoning"
            },
            {
                "type":"message",
                "role":"user",
                "content":[{"type":"input_text","text":"hello"}]
            }
        ],
        "store":false,
        "stream":false
    }))
    .unwrap();

    let response = create(State(state), Json(request)).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    server.await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output_text"], "answer");
}

#[tokio::test]
async fn stored_live_stream_persists_ordered_events_and_final_response() {
    let directory = tempfile::tempdir().unwrap();
    let (state, server) =
        state_with_json_upstream(&directory.path().join("responses.sqlite3")).await;
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"kiro",
        "input":"hello",
        "store":true,
        "stream":true
    }))
    .unwrap();
    let response = create(State(state.clone()), Json(request)).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    server.await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let id = body
        .lines()
        .find(|line| line.starts_with("data: ") && line.contains("response.created"))
        .and_then(|line| serde_json::from_str::<Value>(line.trim_start_matches("data: ")).ok())
        .and_then(|value| value["response"]["id"].as_str().map(ToOwned::to_owned))
        .unwrap();
    let events = state.responses.events(&id).await.unwrap();
    assert!(events.len() >= 5);
    assert_eq!(events.first().unwrap().event_type, "response.created");
    assert!(events.iter().any(|event| event.event_type == "response.output_text.delta"));
    assert!(events.last().unwrap().event_type == "response.completed");
    assert!(events.windows(2).all(|pair| pair[0].sequence_number < pair[1].sequence_number));
    let stored = state.responses.get(&id).await.unwrap().unwrap();
    assert_eq!(stored.status, ResponseStatus::Completed);
    assert!(state.responses.delete(&id).await.unwrap());
    assert!(state.responses.events(&id).await.unwrap().is_empty());
}

#[tokio::test]
async fn live_tool_arguments_and_item_completion_keep_arrival_order() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(&directory.path().join("responses.sqlite3"));
    let internal = crate::generation::GenerationRequest {
        model: "kiro".into(),
        messages: vec![Message::text(crate::generation::Role::User, "lookup")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let upstream: crate::upstream::GenerationEventStream = Box::pin(stream::iter(vec![
        Ok(crate::generation::GenerationEvent::ToolCallStart {
            id: "call_1".into(),
            name: "lookup".into(),
        }),
        Ok(crate::generation::GenerationEvent::ToolCallDelta {
            id: "call_1".into(),
            arguments: "{\"x\":".into(),
            name: None,
        }),
        Ok(crate::generation::GenerationEvent::ToolCallDelta {
            id: "call_1".into(),
            arguments: "1}".into(),
            name: None,
        }),
        Ok(crate::generation::GenerationEvent::ToolCallEnd { id: "call_1".into(), complete: true }),
        Ok(crate::generation::GenerationEvent::Stop { reason: "end_turn".into() }),
    ]));
    let response = responses_live_stream(
        state,
        upstream,
        "resp_test".into(),
        "kiro".into(),
        internal,
        false,
        None,
    )
    .into_response();
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec())
        .unwrap();
    let delta = body.find("response.function_call_arguments.delta").unwrap();
    let done = body.find("response.function_call_arguments.done").unwrap();
    let item_done = body.find("response.output_item.done").unwrap();
    assert!(delta < done && done < item_done);
    assert!(body.contains("\\\"x\\\":1}"));
    assert!(body.contains("response.completed"));
}

#[tokio::test]
async fn live_custom_tool_emits_custom_input_events_and_output_item() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(&directory.path().join("responses.sqlite3"));
    let internal = crate::generation::GenerationRequest {
        model: "kiro".into(),
        messages: vec![Message::text(crate::generation::Role::User, "apply patch")],
        system: None,
        tools: vec![ToolDefinition {
            name: "functions_apply_patch".into(),
            description: Some("Apply a patch".into()),
            input_schema: json!({"type":"object"}),
            custom: true,
            original_name: Some("apply_patch".into()),
            namespace: Some("functions".into()),
        }],
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let upstream: crate::upstream::GenerationEventStream = Box::pin(stream::iter(vec![
        Ok(crate::generation::GenerationEvent::ToolCallStart {
            id: "call_custom".into(),
            name: "functions_apply_patch".into(),
        }),
        Ok(crate::generation::GenerationEvent::ToolCallDelta {
            id: "call_custom".into(),
            arguments: "{\"input\":\"*** Begin\\n+hello\\n*** End\"}".into(),
            name: None,
        }),
        Ok(crate::generation::GenerationEvent::ToolCallEnd {
            id: "call_custom".into(),
            complete: true,
        }),
        Ok(crate::generation::GenerationEvent::Stop { reason: "end_turn".into() }),
    ]));
    let response = responses_live_stream(
        state,
        upstream,
        "resp_custom".into(),
        "kiro".into(),
        internal,
        false,
        None,
    )
    .into_response();
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec())
        .unwrap();
    assert!(body.contains("response.custom_tool_call_input.delta"));
    assert!(body.contains("response.custom_tool_call_input.done"));
    assert!(body.contains("\"type\":\"custom_tool_call\""));
    assert!(body.contains("\"name\":\"apply_patch\""));
    assert!(body.contains("\"namespace\":\"functions\""));
    assert!(body.contains("*** Begin"));
}

#[tokio::test]
async fn live_reasoning_emits_native_responses_summary_events() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(&directory.path().join("responses.sqlite3"));
    let internal = crate::generation::GenerationRequest {
        model: "kiro".into(),
        messages: vec![Message::text(crate::generation::Role::User, "think")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let upstream: crate::upstream::GenerationEventStream = Box::pin(stream::iter(vec![
        Ok(crate::generation::GenerationEvent::ThinkingDelta { text: "plan".into() }),
        Ok(crate::generation::GenerationEvent::TextDelta { text: "answer".into() }),
        Ok(crate::generation::GenerationEvent::Stop { reason: "end_turn".into() }),
    ]));
    let response = responses_live_stream(
        state,
        upstream,
        "resp_reasoning".into(),
        "kiro".into(),
        internal,
        false,
        None,
    )
    .into_response();
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec())
        .unwrap();
    assert!(body.contains("response.reasoning_summary_part.added"));
    assert!(body.contains("response.reasoning_summary_text.delta"));
    assert!(body.contains("response.reasoning_summary_text.done"));
    assert!(body.contains("\"type\":\"reasoning\""));
    assert!(body.contains("\"text\":\"plan\""));
    assert!(body.contains("response.completed"));
}

#[tokio::test]
async fn dropping_a_live_stream_marks_an_in_progress_record_incomplete() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(&directory.path().join("responses.sqlite3"));
    let record = state
        .responses
        .create_with_id(
            "resp_disconnect".into(),
            "kiro",
            json!({"messages":[],"tools":[],"response":{}}),
            ResponseStatus::InProgress,
        )
        .await
        .unwrap();
    let internal = crate::generation::GenerationRequest {
        model: "kiro".into(),
        messages: Vec::new(),
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let upstream: crate::upstream::GenerationEventStream = Box::pin(stream::pending());
    let response = responses_live_stream(
        state.clone(),
        upstream,
        record.id.clone(),
        "kiro".into(),
        internal,
        true,
        Some(record),
    )
    .into_response();
    drop(response);
    assert_eq!(
        state.responses.get("resp_disconnect").await.unwrap().unwrap().status,
        ResponseStatus::Incomplete
    );
    assert!(
        state
            .responses
            .events("resp_disconnect")
            .await
            .unwrap()
            .iter()
            .any(|event| event.event_type == "response.incomplete")
    );
}

#[tokio::test]
async fn missing_previous_response_is_not_treated_as_empty_history() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(&directory.path().join("responses.sqlite3"));
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"kiro",
        "input":"hello",
        "previous_response_id":"resp_missing"
    }))
    .unwrap();
    assert!(matches!(
        create(State(state), Json(request)).await,
        Err(crate::error::AppError::NotFound)
    ));
}

#[test]
fn function_call_item_has_distinct_item_and_call_ids() {
    let payload = responses_payload("resp_test", "kiro", &response("answer"));
    let call = &payload["output"][1];
    assert!(call["id"].as_str().unwrap().starts_with("fc_"));
    assert_ne!(call["id"], call["call_id"]);
    assert_eq!(call["call_id"], "call_upstream");
    assert_eq!(call["arguments"], r#"{"id":42}"#);
    assert_eq!(call["status"], "completed");
}

#[test]
fn custom_tool_response_restores_name_namespace_and_freeform_input() {
    let tools = vec![ToolDefinition {
        name: "functions_apply_patch".into(),
        description: Some("Apply a patch".into()),
        input_schema: json!({"type":"object"}),
        custom: true,
        original_name: Some("apply_patch".into()),
        namespace: Some("functions".into()),
    }];
    let response = GenerationResult {
        tool_calls: vec![ToolCall {
            id: "call_custom".into(),
            name: "functions_apply_patch".into(),
            arguments: json!({"input":"*** Begin\n+hello\n*** End"}),
            complete: true,
        }],
        ..Default::default()
    };
    let payload = responses_payload_with_tools("resp_test", "kiro", &tools, &response);
    let call = &payload["output"][0];
    assert_eq!(call["type"], "custom_tool_call");
    assert!(call["id"].as_str().unwrap().starts_with("ctc_"));
    assert_eq!(call["call_id"], "call_custom");
    assert_eq!(call["name"], "apply_patch");
    assert_eq!(call["namespace"], "functions");
    assert_eq!(call["input"], "*** Begin\n+hello\n*** End");

    let events = responses_stream_events(&payload);
    assert!(events.iter().any(|(kind, _)| *kind == "response.custom_tool_call_input.delta"));
    assert!(events.iter().any(|(kind, _)| *kind == "response.custom_tool_call_input.done"));
}

#[test]
fn reasoning_payload_uses_native_summary_stream_events() {
    let response = GenerationResult {
        text: "answer".into(),
        thinking: "plan".into(),
        stop_reason: Some("end_turn".into()),
        ..Default::default()
    };
    let payload = responses_payload("resp_reasoning", "kiro", &response);
    assert_eq!(payload["output"][0]["type"], "reasoning");
    assert_eq!(payload["output"][0]["summary"][0]["text"], "plan");
    let events = responses_stream_events(&payload);
    let types: Vec<_> = events.iter().map(|event| event.0).collect();
    assert!(types.contains(&"response.reasoning_summary_part.added"));
    assert!(types.contains(&"response.reasoning_summary_text.delta"));
    assert!(types.contains(&"response.reasoning_summary_text.done"));
    assert_eq!(events.last().unwrap().0, "response.completed");
    for (index, (_, event)) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], index as u64);
    }
}

#[test]
fn stream_orders_items_before_final_response() {
    let payload = responses_payload("resp_test", "kiro", &response("answer"));
    let events = responses_stream_events(&payload);
    let types: Vec<_> = events.iter().map(|event| event.0).collect();
    assert_eq!(types.first(), Some(&"response.created"));
    assert_eq!(types.get(1), Some(&"response.in_progress"));
    assert_eq!(types.last(), Some(&"response.completed"));
    let call_added =
        types.iter().position(|kind| *kind == "response.function_call_arguments.delta").unwrap();
    let call_done =
        types.iter().position(|kind| *kind == "response.function_call_arguments.done").unwrap();
    let item_done = types.iter().rposition(|kind| *kind == "response.output_item.done").unwrap();
    assert!(call_added < call_done && call_done < item_done && item_done < types.len() - 1);
    let call_event = &events[call_added].1;
    assert_eq!(call_event["output_index"], 1);
    assert_eq!(call_event["item_id"], payload["output"][1]["id"]);
    assert_eq!(events.last().unwrap().1["response"], payload);
    for (index, (_, event)) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], index as u64);
    }
}

#[test]
fn tool_only_stream_has_no_text_delta_and_can_finish_incomplete() {
    let mut response = response("");
    response.incomplete = true;
    response.tool_calls[0].complete = false;
    let payload = responses_payload("resp_test", "kiro", &response);
    let events = responses_stream_events(&payload);
    assert!(!events.iter().any(|event| event.0 == "response.output_text.delta"));
    assert_eq!(events[2].1["output_index"], 0);
    assert_eq!(events.last().unwrap().0, "response.incomplete");
    assert_eq!(events.last().unwrap().1["response"]["status"], "incomplete");
}

#[test]
fn incomplete_payload_exposes_reason_and_stream_call_identity() {
    let mut response = response("");
    response.stop_reason = Some("MAX_TOKENS".into());
    response.incomplete = true;
    let payload = responses_payload("resp_test", "kiro", &response);
    assert_eq!(payload["status"], "incomplete");
    assert_eq!(payload["incomplete_details"]["reason"], "max_output_tokens");
    let events = responses_stream_events(&payload);
    let delta =
        events.iter().find(|(kind, _)| *kind == "response.function_call_arguments.delta").unwrap();
    assert_eq!(delta.1["call_id"], "call_upstream");
    assert_eq!(delta.1["item_id"], payload["output"][0]["id"]);
    assert_eq!(delta.1["output_index"], 0);
}

#[test]
fn empty_response_has_a_message_output_item() {
    let payload = responses_payload("resp_test", "kiro", &GenerationResult::default());
    assert_eq!(payload["output"].as_array().unwrap().len(), 1);
    assert_eq!(payload["output"][0]["type"], "message");
    assert_eq!(payload["output"][0]["content"][0]["text"], "");
}

#[tokio::test]
async fn stored_response_replays_tool_call_for_previous_response_id() {
    let input = vec![Message::text(crate::generation::Role::User, "question")];
    let messages = response_messages(&input, &response(""));
    let directory = tempfile::tempdir().unwrap();
    let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
    let record = store
        .create("kiro", json!({"messages":messages}), ResponseStatus::Completed)
        .await
        .unwrap();
    let replayed =
        ResponseStore::extract_messages(&store.get(&record.id).await.unwrap().unwrap()).unwrap();
    let continuation: ResponsesRequest = serde_json::from_value(json!({
        "model":"kiro",
        "input":[{
            "type":"function_call_output",
            "call_id":"call_upstream",
            "output":"found"
        }],
        "previous_response_id":record.id
    }))
    .unwrap();
    let internal = continuation.into_generation(replayed).unwrap();
    assert_eq!(internal.messages.len(), 3);
    assert_eq!(internal.messages[1].tool_calls[0].id, "call_upstream");
    assert_eq!(internal.messages[1].tool_calls[0].arguments["id"], 42);
    assert_eq!(internal.messages[2].tool_results[0].tool_call_id, "call_upstream");
    assert_eq!(internal.messages[2].tool_results[0].content, "found");
}

#[tokio::test]
async fn stored_response_replays_tool_definitions_for_continuation() {
    use crate::generation::ToolDefinition;
    let input = vec![Message::text(crate::generation::Role::User, "question")];
    let messages = response_messages(&input, &response(""));
    let directory = tempfile::tempdir().unwrap();
    let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
    let record = store
            .create(
                "kiro",
                json!({
                    "messages":messages,
                    "tools":[ToolDefinition { name:"lookup".into(), description:None, input_schema:json!({"type":"object"}), custom:false, original_name:None, namespace:None }]
                }),
                ResponseStatus::Completed,
            )
            .await
            .unwrap();
    let recovered =
        ResponseStore::extract_tools(&store.get(&record.id).await.unwrap().unwrap()).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].name, "lookup");
}
