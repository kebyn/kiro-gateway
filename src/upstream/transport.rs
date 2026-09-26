use crate::{
    auth::{AuthMethod, Credential},
    endpoint::EndpointAdapter,
    error::AppError,
    generation::GenerationRequest,
};
use futures_util::StreamExt;
use reqwest::Client;

const ERROR_BODY_PREVIEW_BYTES: usize = 4096;

pub(crate) enum SendError {
    Transport(String),
    Application(AppError),
}

pub(crate) enum BodyReadError {
    TooLarge,
    Transport(String),
}

/// Reads a successful upstream body without relying on `Content-Length`.
/// Chunked responses and HTTP/2 data frames are bounded by the same limit as
/// responses that advertise a length up front.
pub(crate) async fn read_body_limited(
    response: reqwest::Response,
    max_body_bytes: usize,
) -> Result<Vec<u8>, BodyReadError> {
    if response.content_length().is_some_and(|length| length > max_body_bytes as u64) {
        return Err(BodyReadError::TooLarge);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| BodyReadError::Transport(error.to_string()))?;
        if body.len().saturating_add(chunk.len()) > max_body_bytes {
            return Err(BodyReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl SendError {
    pub(crate) fn into_app_error(self) -> AppError {
        match self {
            Self::Transport(message) => AppError::Upstream(message),
            Self::Application(error) => error,
        }
    }
}

pub(crate) async fn send_once(
    client: &Client,
    endpoint: &EndpointAdapter,
    request: &GenerationRequest,
    credential: &Credential,
    max_body_bytes: usize,
) -> Result<reqwest::Response, SendError> {
    let body = endpoint.transform_api_body(request, credential);
    tracing::debug!(model = %request.model, "sending upstream request");
    let mut builder = client
        .post(endpoint.api_url(credential))
        .bearer_auth(
            credential.access_token.as_ref().map(|value| value.expose_secret()).unwrap_or_default(),
        )
        .json(&body)
        .header("accept", "application/vnd.amazon.eventstream, application/json");
    if matches!(credential.auth_method, AuthMethod::ApiKey) {
        builder = builder.header("tokentype", "API_KEY");
    } else if matches!(credential.auth_method, AuthMethod::Social) {
        builder = builder.header("TokenType", "EXTERNAL_IDP");
    }
    let response = endpoint
        .decorate_api(builder, credential)
        .send()
        .await
        .map_err(|error| SendError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = read_error_preview(response, max_body_bytes).await;
        return Err(SendError::Application(endpoint.classify_error(status, &body)));
    }
    Ok(response)
}

async fn read_error_preview(response: reqwest::Response, max_body_bytes: usize) -> String {
    let limit = max_body_bytes.min(ERROR_BODY_PREVIEW_BYTES);
    if limit == 0 {
        return String::new();
    }
    let mut body = Vec::with_capacity(limit);
    let mut stream = response.bytes_stream();
    while body.len() < limit {
        let Some(chunk) = stream.next().await else { break };
        let Ok(chunk) = chunk else { break };
        let remaining = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    String::from_utf8_lossy(&body).into_owned()
}
