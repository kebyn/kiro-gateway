use crate::{AppState, error::AppError};
use serde_json::{Value, json};

pub(super) async fn attach_sequence(
    state: &AppState,
    response_id: &str,
    store: bool,
    sequence: &mut u64,
    event_type: &'static str,
    mut payload: Value,
) -> Result<Value, AppError> {
    payload["type"] = Value::String(event_type.to_owned());
    let assigned = if store {
        state.responses.append_event(response_id, event_type, &payload).await?.sequence_number
    } else {
        *sequence
    };
    payload["sequence_number"] = json!(assigned);
    *sequence = assigned.saturating_add(1);
    Ok(payload)
}

pub(super) fn responses_stream_events(payload: &Value) -> Vec<(&'static str, Value)> {
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
            Some("reasoning") => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["summary"] = json!([]);
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.added",
                    json!({"output_index":output_index,"item":added}),
                );
                let summary = item["summary"]
                    .as_array()
                    .and_then(|parts| parts.first())
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"summary_text","text":""}));
                let summary_text = summary["text"].as_str().unwrap_or_default();
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.reasoning_summary_part.added",
                    json!({
                        "item_id":item["id"],
                        "output_index":output_index,
                        "summary_index":0,
                        "part":{"type":"summary_text","text":""}
                    }),
                );
                if !summary_text.is_empty() {
                    push_event(
                        &mut events,
                        &mut sequence_number,
                        "response.reasoning_summary_text.delta",
                        json!({
                            "item_id":item["id"],
                            "output_index":output_index,
                            "summary_index":0,
                            "delta":summary_text
                        }),
                    );
                }
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.reasoning_summary_text.done",
                    json!({
                        "item_id":item["id"],
                        "output_index":output_index,
                        "summary_index":0,
                        "text":summary_text
                    }),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.reasoning_summary_part.done",
                    json!({
                        "item_id":item["id"],
                        "output_index":output_index,
                        "summary_index":0,
                        "part":summary
                    }),
                );
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.done",
                    json!({"output_index":output_index,"item":item}),
                );
            }
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
            Some("custom_tool_call") => {
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["input"] = json!("");
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.output_item.added",
                    json!({"output_index":output_index,"item":added}),
                );
                let input = item["input"].as_str().unwrap_or_default();
                if !input.is_empty() {
                    push_event(
                        &mut events,
                        &mut sequence_number,
                        "response.custom_tool_call_input.delta",
                        json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"delta":input}),
                    );
                }
                push_event(
                    &mut events,
                    &mut sequence_number,
                    "response.custom_tool_call_input.done",
                    json!({"item_id":item["id"],"call_id":item["call_id"],"output_index":output_index,"input":input}),
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
