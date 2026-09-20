use crate::{
    AppState,
    admin::middleware::{clear_session_cookie, parse_session_cookie, session_cookie},
    common::auth::constant_time_eq,
    error::AppError,
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct LoginRequest {
    pub api_key: String,
}
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        if !state.config.admin.allowed_origins.is_empty()
            && !state.config.admin.allowed_origins.iter().any(|allowed| allowed == origin)
        {
            return Err(AppError::Forbidden);
        }
    }
    let client_key = login_rate_limit_key(&headers, state.config.trust_forwarded_headers);
    if !state.sessions.allow_login(&client_key, state.config.admin.login_rate_limit_per_minute) {
        return Err(AppError::RateLimited);
    }
    if !constant_time_eq(&body.api_key, &state.config.admin_api_key) {
        return Err(AppError::Unauthorized);
    }
    let session =
        state.sessions.create(std::time::Duration::from_secs(state.config.admin.session_ttl_secs));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        session_cookie(
            &session.token,
            state.config.admin.cookie_secure,
            state.config.admin.session_ttl_secs,
        )
        .parse()
        .map_err(|_| AppError::Internal("failed to construct session cookie".into()))?,
    );
    Ok((StatusCode::NO_CONTENT, headers))
}
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(token) =
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(parse_session_cookie)
    {
        state.sessions.remove(&token);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        clear_session_cookie(state.config.admin.cookie_secure)
            .parse()
            .map_err(|_| AppError::Internal("failed to construct session cookie".into()))?,
    );
    Ok(response)
}
pub async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_session_cookie)
        .ok_or(AppError::Unauthorized)?;
    let value = state.sessions.get(&token).ok_or(AppError::Unauthorized)?;
    Ok(Json(
        serde_json::json!({"authenticated":true,"expires_at_secs": value.expires_at.saturating_duration_since(std::time::Instant::now()).as_secs(), "csrf_token": value.csrf_token}),
    ))
}
pub async fn credential(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        serde_json::json!({"credential": state.token_manager.status(), "refresh": state.token_manager.refresh_state()}),
    ))
}
pub async fn credential_reload(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    state.reload_credential().await?;
    Ok(Json(serde_json::json!({"ok":true,"credential":state.token_manager.status()})))
}
pub async fn credential_refresh(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    state.token_manager.refresh().await?;
    Ok(Json(serde_json::json!({"ok":true,"credential":state.token_manager.status()})))
}
pub async fn response(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    state.responses.get(&id)?.map(|record| Json(record).into_response()).ok_or(AppError::NotFound)
}
pub async fn delete_response(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if state.responses.delete(&id)? { Ok(StatusCode::NO_CONTENT) } else { Err(AppError::NotFound) }
}

pub async fn response_events(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if state.responses.get(&id)?.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(Json(state.responses.events(&id)?))
}

fn login_rate_limit_key(headers: &HeaderMap, trust_forwarded_headers: bool) -> String {
    if trust_forwarded_headers {
        headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("global")
            .to_owned()
    } else {
        "global".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::login_rate_limit_key;
    use axum::http::HeaderMap;

    #[test]
    fn forwarded_headers_are_only_used_when_explicitly_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.10".parse().unwrap());
        assert_eq!(login_rate_limit_key(&headers, false), "global");
        assert_eq!(login_rate_limit_key(&headers, true), "203.0.113.10");
    }

    #[test]
    fn empty_forwarded_header_uses_global_limit_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", " ".parse().unwrap());
        assert_eq!(login_rate_limit_key(&headers, true), "global");
    }
}
