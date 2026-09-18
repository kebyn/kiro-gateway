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
    json!({"id": format!("msg_{}", uuid::Uuid::now_v7()), "type":"message", "role":"assistant", "model":model, "content":content, "stop_reason": response.stop_reason.clone().unwrap_or_else(|| if response.tool_calls.is_empty() { "end_turn".into() } else { "tool_use".into() }), "stop_sequence":null, "usage": anthropic_usage(response.usage.clone().unwrap_or_else(|| Usage::new(0, 0)))})
}
pub fn anthropic_usage(usage: Usage) -> Value {
    json!({"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens})
}

pub fn openai_chat_response(model: &str, response: &InternalResponse) -> Value {
    let tool_calls: Vec<Value> = response.tool_calls.iter().enumerate().map(|(index, call)| json!({"id":call.id,"type":"function","index":index,"function":{"name":call.name,"arguments":call.arguments.to_string()}})).collect();
    json!({"id":format!("chatcmpl-{}",uuid::Uuid::now_v7()),"object":"chat.completion","created":chrono::Utc::now().timestamp(),"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":if response.text.is_empty(){Value::Null}else{Value::String(response.text.clone())},"tool_calls":tool_calls},"finish_reason":if response.tool_calls.is_empty(){"stop"}else{"tool_calls"}}],"usage":openai_usage(response.usage.clone().unwrap_or_else(||Usage::new(0,0)))})
}
pub fn openai_usage(usage: Usage) -> Value {
    json!({"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"total_tokens":usage.total_tokens})
}
