use crate::{
    AppState,
    error::AppError,
    protocol::{
        internal::{InternalRequest, InternalResponse},
        openai_chat::ChatRequest,
    },
    transform::converter::{chat_finish_reason, openai_chat_response},
};
use axum::{
    Json,
    extract::State,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::stream;
use serde_json::json;

pub async fn chat_completions(
    State(state): State<AppState>,
    Json(body): Json<ChatRequest>,
) -> Result<Response, AppError> {
    let request: InternalRequest = body.into();
    let stream_response = request.stream;
    let model = request.model.clone();
    let response = state.complete(&request).await?;
    let payload = openai_chat_response(&model, &response);
    if stream_response {
        let events = chat_stream_data(&payload, &response)
            .into_iter()
            .map(|data| Event::default().data(data));
        Ok(Sse::new(stream::iter(events.map(Ok::<Event, std::convert::Infallible>)))
            .into_response())
    } else {
        Ok(Json(payload).into_response())
    }
}

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
    use super::chat_stream_data;
    use crate::{
        protocol::internal::{InternalResponse, InternalToolCall},
        transform::converter::openai_chat_response,
    };
    use serde_json::{Value, json};

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
