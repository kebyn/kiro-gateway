use crate::{
    AppState,
    admin::middleware::{clear_session_cookie, session_cookie},
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
    let client_key =
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("global");
    if !state.sessions.allow_login(client_key, state.config.admin.login_rate_limit_per_minute) {
        return Err(AppError::Forbidden);
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
        .unwrap(),
    );
    Ok((StatusCode::NO_CONTENT, headers))
}
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|v| {
        v.split(';')
            .find_map(|p| p.trim().strip_prefix("kiro_admin_session=").map(ToOwned::to_owned))
    }) {
        state.sessions.remove(&token);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        clear_session_cookie(state.config.admin.cookie_secure).parse().unwrap(),
    );
    response
}
pub async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';')
                .find_map(|p| p.trim().strip_prefix("kiro_admin_session=").map(ToOwned::to_owned))
        })
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
pub async fn request_logs() -> impl IntoResponse {
    Json(serde_json::json!({"items": []}))
}
pub async fn clear_request_logs() -> impl IntoResponse {
    StatusCode::NO_CONTENT
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
