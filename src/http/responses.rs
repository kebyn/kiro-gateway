use crate::{
    AppState,
    error::AppError,
    protocol::{
        internal::{InternalMessage, InternalResponse},
        openai_responses::ResponsesRequest,
    },
    response_store::{ResponseStatus, ResponseStore},
    transform::converter::responses_incomplete_reason,
};
use axum::{
    Json,
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::stream;
use serde_json::{Value, json};

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<ResponsesRequest>,
) -> Result<Response, AppError> {
    let previous = body
        .previous_response_id
        .as_deref()
        .and_then(|id| state.responses.get(id).ok().flatten())
        .map(|record| ResponseStore::extract_messages(&record))
        .unwrap_or_default();
    let store = body.store;
    let stream_response = body.stream;
    let internal = body.into_internal(previous);
    let model = internal.model.clone();
    let response = state.complete(&internal).await?;
    let id = format!("resp_{}", uuid::Uuid::now_v7());
    let payload = responses_payload(&id, &model, &response);
    if store {
        let stored_messages = response_messages(&internal.messages, &response);
        let mut record = state.responses.create_with_id(
            id.clone(),
            &model,
            json!({"messages":stored_messages,"response":payload}),
            ResponseStatus::InProgress,
        )?;
        let status = if response.incomplete {
            ResponseStatus::Incomplete
        } else {
            ResponseStatus::Completed
        };
        record = state.responses.update(
            record,
            status,
            json!({"messages":stored_messages,"response":payload}),
        )?;
        let _ = record;
    }
    if stream_response {
        let events = responses_stream_events(&payload)
            .into_iter()
            .map(|(event, data)| Event::default().event(event).data(data.to_string()));
        Ok(Sse::new(stream::iter(events.into_iter().map(Ok::<Event, std::convert::Infallible>)))
            .into_response())
    } else {
        Ok(Json(payload).into_response())
    }
}

fn responses_payload(id: &str, model: &str, response: &InternalResponse) -> Value {
    let incomplete_reason = responses_incomplete_reason(response);
    let status = if incomplete_reason.is_some() { "incomplete" } else { "completed" };
    let mut output = Vec::new();
    if !response.text.is_empty() {
        output.push(json!({
            "type":"message",
            "id":format!("msg_{}", uuid::Uuid::now_v7()),
            "status":status,
            "role":"assistant",
            "content":[{
                "type":"output_text",
                "text":response.text,
                "annotations":[],
                "logprobs":[]
            }]
        }));
    }
    for call in &response.tool_calls {
        output.push(json!({
            "type":"function_call",
            "id":format!("fc_{}", uuid::Uuid::now_v7()),
            "call_id":call.id,
            "name":call.name,
            "arguments":call.arguments_json(),
            "status":if call.complete { "completed" } else { "incomplete" }
        }));
    }
    if output.is_empty() {
        output.push(json!({
            "type":"message",
            "id":format!("msg_{}", uuid::Uuid::now_v7()),
            "status":status,
            "role":"assistant",
            "content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]
        }));
    }
    json!({
        "id":id,
        "object":"response",
        "created_at":chrono::Utc::now().timestamp(),
        "status":status,
        "error":null,
        "incomplete_details":incomplete_reason.map(|reason| json!({"reason":reason})),
        "model":model,
        "output":output,
        "output_text":response.text,
        "usage":response.usage
    })
}

fn response_messages(
    input: &[InternalMessage],
    response: &InternalResponse,
) -> Vec<InternalMessage> {
    let mut messages = input.to_vec();
    if !response.text.is_empty() || !response.tool_calls.is_empty() {
        let mut assistant = InternalMessage::new(
            "assistant",
            if response.text.is_empty() {
                Value::Null
            } else {
                Value::String(response.text.clone())
            },
        );
        assistant.tool_calls = response.tool_calls.clone();
        messages.push(assistant);
    }
    messages
}

fn responses_stream_events(payload: &Value) -> Vec<(&'static str, Value)> {
    let mut events = Vec::new();
    let mut sequence_number = 0_u64;
    let mut created = payload.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["output_text"] = json!("");
    push_event(
        &mut events,
        &mut sequence_number,
        "response.created",
        json!({"response":created.clone()}),
    );
    push_event(
        &mut events,
        &mut sequence_number,
        "response.in_progress",
        json!({"response":created}),
    );

    for (output_index, item) in payload["output"].as_array().into_iter().flatten().enumerate() {
        match item["type"].as_str() {
            Some("message") => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["content"] = json!([]);
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.added",
                    json!({"output_index":output_index,"item":added}),
                );
                let part = item["content"][0].clone();
                let mut empty_part = part.clone();
                empty_part["text"] = json!("");
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.content_part.added",
                    json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"part":empty_part}),
                );
                if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                    push_event(
                        &mut events,
                        &mut sequence_number,
                        "response.output_text.delta",
                        json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"delta":text,"logprobs":[]}),
                    );
                }
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_text.done",
                    json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"text":part["text"],"logprobs":[]}),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.content_part.done",
                    json!({"item_id":item["id"],"output_index":output_index,"content_index":0,"part":part}),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.done",
                    json!({"output_index":output_index,"item":item}),
                );
            }
            Some("function_call") => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["arguments"] = json!("");
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.added",
                    json!({"output_index":output_index,"item":added}),
                );
                let arguments = item["arguments"].as_str().unwrap_or_default();
                if !arguments.is_empty() {
                    push_event(
                        &mut events,
                        &mut sequence_number,
                        "response.function_call_arguments.delta",
                        json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"delta":arguments}),
                    );
                }
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.function_call_arguments.done",
                    json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"arguments":arguments}),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.done",
                    json!({"output_index":output_index,"item":item}),
                );
            }
            _ => {}
        }
    }
    let terminal = if payload["status"] == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    push_event(&mut events, &mut sequence_number, terminal, json!({"response":payload}));
    events
}

fn push_event(
    events: &mut Vec<(&'static str, Value)>,
    sequence_number: &mut u64,
    event_type: &'static str,
    mut data: Value,
) {
    data["type"] = Value::String(event_type.into());
    data["sequence_number"] = json!(*sequence_number);
    events.push((event_type, data));
    *sequence_number += 1;
}
pub async fn get(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let record = state.responses.get(&id)?.ok_or(AppError::NotFound)?;
    Ok(Json(record.payload.get("response").cloned().unwrap_or(Value::Null)))
}
pub async fn delete(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if state.responses.delete(&id)? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::{response_messages, responses_payload, responses_stream_events};
    use crate::{
        protocol::internal::{InternalMessage, InternalResponse, InternalToolCall},
        protocol::openai_responses::ResponsesRequest,
        response_store::{ResponseStatus, ResponseStore},
    };
    use serde_json::{Value, json};

    fn response(text: &str) -> InternalResponse {
        InternalResponse {
            text: text.into(),
            tool_calls: vec![InternalToolCall {
                id: "call_upstream".into(),
                name: "lookup".into(),
                arguments: json!({"id":42}),
                complete: true,
            }],
            ..Default::default()
        }
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
    fn stream_orders_items_before_final_response() {
        let payload = responses_payload("resp_test", "kiro", &response("answer"));
        let events = responses_stream_events(&payload);
        let types: Vec<_> = events.iter().map(|event| event.0).collect();
        assert_eq!(types.first(), Some(&"response.created"));
        assert_eq!(types.get(1), Some(&"response.in_progress"));
        assert_eq!(types.last(), Some(&"response.completed"));
        let call_added = types
            .iter()
            .position(|kind| *kind == "response.function_call_arguments.delta")
            .unwrap();
        let call_done =
            types.iter().position(|kind| *kind == "response.function_call_arguments.done").unwrap();
        let item_done =
            types.iter().rposition(|kind| *kind == "response.output_item.done").unwrap();
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
        let delta = events
            .iter()
            .find(|(kind, _)| *kind == "response.function_call_arguments.delta")
            .unwrap();
        assert_eq!(delta.1["call_id"], "call_upstream");
        assert_eq!(delta.1["item_id"], payload["output"][0]["id"]);
        assert_eq!(delta.1["output_index"], 0);
    }

    #[test]
    fn empty_response_has_a_message_output_item() {
        let payload = responses_payload("resp_test", "kiro", &InternalResponse::default());
        assert_eq!(payload["output"].as_array().unwrap().len(), 1);
        assert_eq!(payload["output"][0]["type"], "message");
        assert_eq!(payload["output"][0]["content"][0]["text"], "");
    }

    #[test]
    fn stored_response_replays_tool_call_for_previous_response_id() {
        let input = vec![InternalMessage::new("user", Value::String("question".into()))];
        let messages = response_messages(&input, &response(""));
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record =
            store.create("kiro", json!({"messages":messages}), ResponseStatus::Completed).unwrap();
        let replayed = ResponseStore::extract_messages(&store.get(&record.id).unwrap().unwrap());
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
        let internal = continuation.into_internal(replayed);
        assert_eq!(internal.messages.len(), 3);
        assert_eq!(internal.messages[1].tool_calls[0].id, "call_upstream");
        assert_eq!(internal.messages[1].tool_calls[0].arguments["id"], 42);
        assert_eq!(internal.messages[2].tool_results[0].tool_call_id, "call_upstream");
        assert_eq!(internal.messages[2].tool_results[0].content, "found");
    }
}
