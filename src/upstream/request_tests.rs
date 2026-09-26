use super::{UpstreamClient, event_marks_completion, is_empty_stream};
use crate::{
    auth::{AuthMethod, Credential, SecretString},
    endpoint::EndpointPolicy,
    generation::{GenerationEvent, GenerationRequest, GenerationResult},
    transform::truncation::XmlLeakFilter,
    upstream::{
        accumulator::{apply_event, finish_result},
        event_stream::{EventStreamDecoder, decode_generation_events},
        integrity::StreamIntegrity,
        json::{decode_events, parse_tool_calls},
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
    let request = GenerationRequest {
        model: "kiro".into(),
        messages: vec![crate::generation::Message::text(crate::generation::Role::User, "hello")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let mut events = client.event_stream(&request, &credential).await.unwrap();
    let mut collected = Vec::new();
    while let Some(event) = events.next().await {
        collected.push(event.unwrap());
    }
    server.await.unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        matches!(&collected[..], [GenerationEvent::TextDelta { text }, GenerationEvent::Stop { reason }] if text == "ok" && reason == "end_turn")
    );
}

#[tokio::test]
async fn limits_non_success_error_body_preview() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        let body = format!("{{\"message\":\"{}\"}}", "x".repeat(16 * 1024));
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    });
    let client = UpstreamClient::with_policy_and_limit(
        reqwest::Client::new(),
        EndpointPolicy::Cli,
        Some(format!("http://{address}")),
        128,
    );
    let credential = Credential {
        auth_method: AuthMethod::ApiKey,
        access_token: Some(SecretString::new("token")),
        endpoint: "cli".into(),
        ..Default::default()
    };
    let request = GenerationRequest {
        model: "kiro".into(),
        messages: vec![crate::generation::Message::text(crate::generation::Role::User, "hello")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let mut events = client.event_stream(&request, &credential).await.unwrap();
    let error = events.next().await.unwrap().unwrap_err();
    let message = error.to_string();
    assert!(message.contains("CLI endpoint returned 500"));
    assert!(message.len() < 512);
    server.await.unwrap();
}

#[tokio::test]
async fn limits_chunked_success_json_without_content_length() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(b"8\r\n12345678\r\n8\r\nabcdefgh\r\n0\r\n\r\n").await.unwrap();
    });
    let client = UpstreamClient::with_policy_and_limit(
        reqwest::Client::new(),
        EndpointPolicy::Cli,
        Some(format!("http://{address}")),
        12,
    );
    let credential = Credential {
        auth_method: AuthMethod::ApiKey,
        access_token: Some(SecretString::new("token")),
        endpoint: "cli".into(),
        ..Default::default()
    };
    let request = GenerationRequest {
        model: "kiro".into(),
        messages: vec![crate::generation::Message::text(crate::generation::Role::User, "hello")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let mut events = client.event_stream(&request, &credential).await.unwrap();
    let error = events.next().await.unwrap().unwrap_err();
    assert!(error.to_string().contains("exceeds configured body limit"));
    server.await.unwrap();
}

#[tokio::test]
async fn rejects_empty_success_json_response() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let client = UpstreamClient::with_policy(
        reqwest::Client::new(),
        EndpointPolicy::Cli,
        Some(format!("http://{address}")),
    );
    let credential = Credential {
        auth_method: AuthMethod::ApiKey,
        access_token: Some(SecretString::new("token")),
        endpoint: "cli".into(),
        ..Default::default()
    };
    let request = GenerationRequest {
        model: "kiro".into(),
        messages: vec![crate::generation::Message::text(crate::generation::Role::User, "hello")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let mut events = client.event_stream(&request, &credential).await.unwrap();
    let error = events.next().await.unwrap().unwrap_err();
    assert!(error.to_string().contains("empty") || error.to_string().contains("JSON"));
    server.await.unwrap();
}

#[tokio::test]
async fn retries_then_reports_interrupted_success_json_body() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 32\r\nconnection: close\r\n\r\n{\"models\":",
                )
                .await
                .unwrap();
        }
    });
    let client = UpstreamClient::with_policy(
        reqwest::Client::new(),
        EndpointPolicy::Cli,
        Some(format!("http://{address}")),
    );
    let credential = Credential {
        auth_method: AuthMethod::ApiKey,
        access_token: Some(SecretString::new("token")),
        endpoint: "cli".into(),
        ..Default::default()
    };
    let request = GenerationRequest {
        model: "kiro".into(),
        messages: vec![crate::generation::Message::text(crate::generation::Role::User, "hello")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
    };
    let mut events = client.event_stream(&request, &credential).await.unwrap();
    let error = events.next().await.unwrap().unwrap_err();
    assert!(error.to_string().contains("incomplete") || error.to_string().contains("body"));
    server.await.unwrap();
}

#[test]
fn incomplete_terminal_reason_is_not_reported_as_completed() {
    let response = finish_result(
        GenerationResult {
            text: "partial".into(),
            stop_reason: Some("stream_incomplete".into()),
            ..Default::default()
        },
        &mut ToolCallAccumulator::new(),
    );
    assert!(response.incomplete);
    assert!(!event_marks_completion(&GenerationEvent::Stop { reason: "stream_incomplete".into() }));
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
                    request.contains("x-amz-target: KiroRuntimeService.GenerateAssistantResponse")
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
    let request = GenerationRequest {
        model: "ide-model".into(),
        messages: vec![crate::generation::Message::text(crate::generation::Role::User, "hello")],
        system: None,
        tools: Vec::new(),
        stream: true,
        max_tokens: None,
        temperature: None,
        conversation_id: None,
        instructions: None,
        opaque_history: Vec::new(),
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
            [GenerationEvent::TextDelta { text }, GenerationEvent::Stop { reason }]
                if text == "ok" && reason == "end_turn"
        ));
    }
    server.await.unwrap();
}

fn response_from_events(events: Vec<(&str, Value)>) -> GenerationResult {
    let bytes = events
        .into_iter()
        .flat_map(|(kind, payload)| event_frame(kind, payload))
        .collect::<Vec<_>>();
    let mut decoder = EventStreamDecoder::new();
    let mut tools = ToolCallAccumulator::new();
    let mut filter = XmlLeakFilter::new();
    let mut integrity = StreamIntegrity::default();
    let mut output = GenerationResult::default();
    for message in decoder.push(&bytes).unwrap() {
        for event in decode_generation_events(&message).unwrap() {
            apply_event(event, &mut output, &mut tools, &mut filter, &mut integrity).unwrap();
        }
    }
    decoder.finish().unwrap();
    finish_result(output, &mut tools)
}

#[test]
fn parses_parallel_json_tool_uses() {
    let calls = parse_tool_calls(&serde_json::json!({
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
    let calls = parse_tool_calls(&serde_json::json!({
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
fn adapts_json_response_to_ordered_generation_events() {
    let events = decode_events(
            br#"{"content":"answer","thinking":"plan","toolUses":[{"toolUseId":"call_1","name":"lookup","input":"{\"id\":1}"}],"usage":{"inputTokens":2,"outputTokens":3},"stopReason":"end_turn"}"#,
        )
        .unwrap();
    assert!(matches!(events[0], GenerationEvent::TextDelta { .. }));
    assert!(matches!(events[1], GenerationEvent::ThinkingDelta { .. }));
    assert!(matches!(events[2], GenerationEvent::ToolCallStart { .. }));
    assert!(matches!(events[3], GenerationEvent::ToolCallDelta { .. }));
    assert!(matches!(events[4], GenerationEvent::ToolCallEnd { .. }));
    assert!(matches!(events[5], GenerationEvent::Usage { .. }));
    assert!(matches!(events[6], GenerationEvent::Stop { .. }));
}

#[test]
fn adapts_nested_runtime_json_response() {
    let events = decode_events(
            br#"{"assistantResponseEvent":{"content":[{"type":"output_text","text":"nested answer"}],"toolUses":[{"toolUseId":"call_1","name":"lookup","input":{"id":1}}],"usage":{"inputTokens":2,"outputTokens":3},"stopReason":"end_turn"}}"#,
        )
        .unwrap();
    assert!(matches!(
        &events[0],
        GenerationEvent::TextDelta { text } if text == "nested answer"
    ));
    assert!(matches!(
        &events[1],
        GenerationEvent::ToolCallStart { id, name } if id == "call_1" && name == "lookup"
    ));
    assert!(
        matches!(&events[2], GenerationEvent::ToolCallDelta { arguments, .. } if arguments == r#"{"id":1}"#)
    );
    assert!(matches!(&events[3], GenerationEvent::ToolCallEnd { complete: true, .. }));
    assert!(
        matches!(&events[4], GenerationEvent::Usage { usage } if usage.input_tokens == 2 && usage.output_tokens == 3)
    );
    assert!(matches!(
        &events[5],
        GenerationEvent::Stop { reason } if reason == "end_turn"
    ));
}

#[test]
fn adapts_nested_message_content_json_response() {
    let events = decode_events(
        br#"{"response":{"message":{"content":[{"type":"text","text":"deep answer"}]}}}"#,
    )
    .unwrap();
    assert!(matches!(
        &events[..],
        [GenerationEvent::TextDelta { text }, GenerationEvent::Stop { reason }]
            if text == "deep answer" && reason == "end_turn"
    ));
}

#[test]
fn rejects_unrecognized_success_json_instead_of_emitting_empty_stop() {
    let error = decode_events(br#"{"status":"ok","metadata":{"requestId":"redacted"}}"#)
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
    let error = decode_events(&body).expect_err("deep upstream JSON must be rejected");
    assert!(error.to_string().contains("nesting"));
}

#[test]
fn rejects_generic_message_json_instead_of_emitting_empty_stop() {
    let error = decode_events(br#"{"message":"completed","metadata":{"requestId":"redacted"}}"#)
        .expect_err("generic message JSON must not become an empty response");
    assert!(matches!(error, crate::error::AppError::Integrity(_)));
}

#[test]
fn preserves_explicit_empty_runtime_response() {
    let events =
        decode_events(br#"{"assistantResponseEvent":{"content":"","stopReason":"end_turn"}}"#)
            .unwrap();
    assert!(matches!(
        &events[..],
        [GenerationEvent::Stop { reason }] if reason == "end_turn"
    ));
}

#[test]
fn preserves_explicit_empty_completed_response() {
    let events = decode_events(br#"{"response":{"status":"completed","output":[]}}"#).unwrap();
    assert!(matches!(
        &events[..],
        [GenerationEvent::Stop { reason }] if reason == "end_turn"
    ));
}

#[test]
fn adapts_json_upstream_error_to_error_event() {
    let events = decode_events(br#"{"error":{"message":"overloaded"}}"#).unwrap();
    assert!(matches!(&events[..], [GenerationEvent::Error { message }] if message == "overloaded"));
}

#[test]
fn adapts_cli_json_error_envelope_to_error_event() {
    let events = decode_events(
        br#"{"Output":{"__type":"ModelError","message":"request rejected"},"Version":"1.0"}"#,
    )
    .unwrap();
    assert!(matches!(
        &events[..],
        [GenerationEvent::Error { message }] if message == "ModelError: request rejected"
    ));
}

#[test]
fn adapts_nested_output_text_and_openai_function_calls() {
    let events = decode_events(
            br#"{"output":[{"type":"message","content":[{"type":"output_text","text":"nested"}]},{"type":"function","id":"call_1","function":{"name":"lookup","arguments":"{\"x\":1}"}}]}"#,
        )
        .unwrap();
    assert!(matches!(&events[0], GenerationEvent::TextDelta { text } if text == "nested"));
    assert!(
        matches!(&events[1], GenerationEvent::ToolCallStart { id, name } if id == "call_1" && name == "lookup")
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
        assert_eq!(response.stop_reason.as_ref().map(|reason| reason.as_str()), Some("end_turn"));
        assert!(!response.incomplete);
    }
}

#[test]
fn completion_logging_tracks_terminal_generation_events() {
    assert!(event_marks_completion(&GenerationEvent::Stop { reason: "end_turn".into() }));
    assert!(event_marks_completion(&GenerationEvent::ToolCallEnd {
        id: "call_1".into(),
        complete: true,
    }));
    assert!(!event_marks_completion(&GenerationEvent::ToolCallEnd {
        id: "call_1".into(),
        complete: false,
    }));
    assert!(!event_marks_completion(&GenerationEvent::TextDelta { text: "answer".into() }));
}

#[test]
fn rejects_a_clean_but_empty_stream_without_terminal_signal() {
    assert!(is_empty_stream(&GenerationResult::default()));
    assert!(!is_empty_stream(&GenerationResult {
        stop_reason: Some("end_turn".into()),
        ..Default::default()
    }));
}
