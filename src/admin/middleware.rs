use crate::{
    AppState,
    common::auth::{constant_time_eq, extract_api_key, is_safe_method},
    error::{AppError, Protocol, protocol_error_response},
};
use axum::{
    extract::{Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

pub async fn client_api_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if extract_api_key(request.headers())
        .is_some_and(|key| constant_time_eq(&key, &state.config.client_api_key))
    {
        Ok(next.run(request).await)
    } else {
        let path = request.uri().path();
        let protocol = if matches!(path, "/v1/messages" | "/v1/messages/count_tokens") {
            Some(Protocol::Anthropic)
        } else if path == "/v1/chat/completions" {
            Some(Protocol::ChatCompletions)
        } else if path == "/v1/responses" || path.starts_with("/v1/responses/") {
            Some(Protocol::Responses)
        } else {
            None
        };
        match protocol {
            Some(protocol) => Ok(protocol_error_response(protocol, AppError::Unauthorized)),
            None => Err(AppError::Unauthorized),
        }
    }
}

pub async fn admin_session(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if let Some(origin) = request.headers().get("origin").and_then(|v| v.to_str().ok()) {
        if !state.config.admin.allowed_origins.is_empty()
            && !state.config.admin.allowed_origins.iter().any(|allowed| allowed == origin)
        {
            return Err(AppError::Forbidden);
        }
    }
    let cookie = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_session_cookie)
        .ok_or(AppError::Unauthorized)?;
    let session = state.sessions.get(&cookie).ok_or(AppError::Unauthorized)?;
    if !is_safe_method(request.method()) {
        let csrf =
            request.headers().get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or_default();
        if !constant_time_eq(csrf, &session.csrf_token) {
            return Err(AppError::Forbidden);
        }
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "x-csrf-token",
        HeaderValue::from_str(&session.csrf_token).unwrap_or(HeaderValue::from_static("")),
    );
    Ok(response)
}

pub(crate) fn parse_session_cookie(value: &str) -> Option<String> {
    value
        .split(';')
        .find_map(|part| part.trim().strip_prefix("kiro_admin_session=").map(ToOwned::to_owned))
}

pub fn session_cookie(token: &str, secure: bool, ttl_secs: u64) -> String {
    format!(
        "kiro_admin_session={token}; Path=/; Max-Age={ttl_secs}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}
pub fn clear_session_cookie(secure: bool) -> String {
    format!(
        "kiro_admin_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}
