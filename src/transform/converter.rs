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
    if response.tool_calls.is_empty() {
        response.stop_reason.clone().unwrap_or_else(|| "end_turn".into())
    } else {
        "tool_use".into()
    }
}

pub fn chat_finish_reason(response: &InternalResponse) -> &'static str {
    if response.tool_calls.is_empty() { "stop" } else { "tool_calls" }
}

#[cfg(test)]
mod tests {
    use super::{anthropic_response, openai_chat_response};
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
}
