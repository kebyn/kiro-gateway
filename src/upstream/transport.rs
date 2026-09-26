use crate::{
    auth::{AuthMethod, Credential},
    endpoint::EndpointAdapter,
    error::AppError,
    generation::GenerationRequest,
};
use reqwest::Client;

pub(crate) enum SendError {
    Transport(String),
    Application(AppError),
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
        let body = response
            .bytes()
            .await
            .map(|body| String::from_utf8_lossy(&body[..body.len().min(4096)]).into_owned())
            .unwrap_or_default();
        return Err(SendError::Application(endpoint.classify_error(status, &body)));
    }
    Ok(response)
}
