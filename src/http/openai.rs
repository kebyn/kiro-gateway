use crate::{
    AppState,
    error::AppError,
    protocol::{internal::InternalRequest, openai_chat::ChatRequest},
    transform::converter::openai_chat_response,
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
        let content = response.text;
        let chunk = Event::default().data(json!({"id":payload["id"],"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":content},"finish_reason":null}]}).to_string());
        let done = Event::default().data("[DONE]");
        Ok(Sse::new(stream::iter(vec![Ok::<Event, std::convert::Infallible>(chunk), Ok(done)]))
            .into_response())
    } else {
        Ok(Json(payload).into_response())
    }
}
