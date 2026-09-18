use crate::protocol::internal::{InternalResponse, Usage};
use serde_json::{Value, json};

pub fn anthropic_response(model: &str, response: &InternalResponse) -> Value {
    let mut content = Vec::new();
    if !response.thinking.is_empty() {
        content.push(json!({"type":"thinking", "thinking": response.thinking}));
    }
    if !response.text.is_empty() {
        content.push(json!({"type":"text", "text": response.text}));
    }
    for call in &response.tool_calls {
        content.push(
            json!({"type":"tool_use", "id": call.id, "name": call.name, "input": call.arguments}),
        );
    }
    json!({"id": format!("msg_{}", uuid::Uuid::now_v7()), "type":"message", "role":"assistant", "model":model, "content":content, "stop_reason": anthropic_stop_reason(response), "stop_sequence":null, "usage": anthropic_usage(response.usage.clone().unwrap_or_else(|| Usage::new(0, 0)))})
}
pub fn anthropic_usage(usage: Usage) -> Value {
    json!({"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens})
}

pub fn openai_chat_response(model: &str, response: &InternalResponse) -> Value {
    let tool_calls: Vec<Value> = response.tool_calls.iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments_json()}})).collect();
    json!({"id":format!("chatcmpl-{}",uuid::Uuid::now_v7()),"object":"chat.completion","created":chrono::Utc::now().timestamp(),"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":if response.text.is_empty(){Value::Null}else{Value::String(response.text.clone())},"tool_calls":tool_calls},"finish_reason":chat_finish_reason(response)}],"usage":openai_usage(response.usage.clone().unwrap_or_else(||Usage::new(0,0)))})
}
pub fn openai_usage(usage: Usage) -> Value {
    json!({"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"total_tokens":usage.total_tokens})
}

pub fn anthropic_stop_reason(response: &InternalResponse) -> String {
    if !response.tool_calls.is_empty() {
        return "tool_use".into();
    }
    if response.incomplete {
        return "max_tokens".into();
    }
    match normalized_stop_reason(response.stop_reason.as_deref()) {
        Some("max_tokens") => "max_tokens".into(),
        Some("context_window_exceeded") => "model_context_window_exceeded".into(),
        Some("refusal") => "refusal".into(),
        Some("stop_sequence") => "stop_sequence".into(),
        Some("pause_turn") => "pause_turn".into(),
        _ => "end_turn".into(),
    }
}

pub fn chat_finish_reason(response: &InternalResponse) -> &'static str {
    if !response.tool_calls.is_empty() {
        return "tool_calls";
    }
    if response.incomplete {
        return "length";
    }
    match normalized_stop_reason(response.stop_reason.as_deref()) {
        Some("max_tokens") | Some("context_window_exceeded") => "length",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

pub fn normalized_stop_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason?.trim().to_ascii_lowercase().as_str() {
        "max_tokens" | "max_output_tokens" | "length" => Some("max_tokens"),
        "model_context_window_exceeded" | "context_window_exceeded" | "context_limit" => {
            Some("context_window_exceeded")
        }
        "refusal" | "content_filter" | "content_filtered" | "guardrail_intervened" => {
            Some("refusal")
        }
        "stop_sequence" => Some("stop_sequence"),
        "pause_turn" => Some("pause_turn"),
        "end_turn" | "complete" | "completed" | "stop" => Some("end_turn"),
        _ => None,
    }
}

pub fn responses_incomplete_reason(response: &InternalResponse) -> Option<&'static str> {
    match normalized_stop_reason(response.stop_reason.as_deref()) {
        Some("max_tokens") | Some("context_window_exceeded") => Some("max_output_tokens"),
        Some("refusal") => Some("content_filter"),
        _ if response.incomplete => Some("max_output_tokens"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{anthropic_response, anthropic_stop_reason, openai_chat_response};
    use crate::protocol::internal::{InternalResponse, InternalToolCall};
    use serde_json::json;

    fn tool_response() -> InternalResponse {
        InternalResponse {
            text: "Let me check".into(),
            tool_calls: vec![InternalToolCall {
                id: "call_weather".into(),
                name: "weather".into(),
                arguments: json!({"city":"Paris"}),
                complete: true,
            }],
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        }
    }

    #[test]
    fn anthropic_non_stream_response_contains_mixed_tool_output() {
        let payload = anthropic_response("kiro", &tool_response());
        assert_eq!(payload["content"][0]["text"], "Let me check");
        assert_eq!(payload["content"][1]["type"], "tool_use");
        assert_eq!(payload["content"][1]["input"]["city"], "Paris");
        assert_eq!(payload["stop_reason"], "tool_use");
    }

    #[test]
    fn chat_non_stream_response_uses_json_argument_string() {
        let payload = openai_chat_response("kiro", &tool_response());
        let call = &payload["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "call_weather");
        assert_eq!(call["function"]["arguments"], r#"{"city":"Paris"}"#);
        assert!(call.get("index").is_none());
        assert_eq!(payload["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn maps_upstream_stop_reasons_to_protocol_values() {
        let mut response =
            InternalResponse { stop_reason: Some("MAX_TOKENS".into()), ..Default::default() };
        assert_eq!(anthropic_stop_reason(&response), "max_tokens");
        assert_eq!(
            openai_chat_response("kiro", &response)["choices"][0]["finish_reason"],
            "length"
        );
        response.stop_reason = Some("CONTENT_FILTERED".into());
        assert_eq!(anthropic_stop_reason(&response), "refusal");
        assert_eq!(
            openai_chat_response("kiro", &response)["choices"][0]["finish_reason"],
            "content_filter"
        );
        response.stop_reason = None;
        response.incomplete = true;
        assert_eq!(anthropic_stop_reason(&response), "max_tokens");
        assert_eq!(
            openai_chat_response("kiro", &response)["choices"][0]["finish_reason"],
            "length"
        );
    }
}
