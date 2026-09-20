use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("rate limit exceeded")]
    RateLimited,
    #[error("request body too large")]
    PayloadTooLarge,
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error("credential error: {0}")]
    Credential(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("stream integrity error: {0}")]
    Integrity(String),
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        generic_error_response(self)
    }
}

fn error_status(error: &AppError) -> StatusCode {
    match error {
        AppError::Unauthorized => StatusCode::UNAUTHORIZED,
        AppError::Forbidden => StatusCode::FORBIDDEN,
        AppError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        AppError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        AppError::NotFound => StatusCode::NOT_FOUND,
        AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
        AppError::Credential(_) | AppError::Upstream(_) | AppError::Integrity(_) => {
            StatusCode::BAD_GATEWAY
        }
        AppError::Config(_) | AppError::Storage(_) | AppError::Internal(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

fn error_kind(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::PAYLOAD_TOO_LARGE => "invalid_request_error",
        StatusCode::BAD_REQUEST => "invalid_request_error",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::BAD_GATEWAY => "upstream_error",
        _ => "server_error",
    }
}

fn safe_message(error: &AppError) -> String {
    match error {
        AppError::Storage(_) | AppError::Internal(_) | AppError::Config(_) => {
            "internal server error".into()
        }
        _ => error.to_string(),
    }
}

fn generic_error_response(error: AppError) -> Response {
    let status = error_status(&error);
    let kind = error_kind(status);
    let message = safe_message(&error);
    (status, axum::Json(ErrorBody { error: ErrorDetail { message: &message, kind } }))
        .into_response()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Anthropic,
    ChatCompletions,
    Responses,
}

/// Maps an application error to the protocol-specific non-streaming error
/// envelope. Streaming handlers use the same classification when emitting an
/// SSE error event.
pub fn protocol_error_response(protocol: Protocol, error: AppError) -> Response {
    let status = error.status_code();
    let message = safe_message(&error);
    let (body, content_type) = match protocol {
        Protocol::Anthropic => (
            serde_json::json!({
                "type": "error",
                "error": {"type": anthropic_error_type(status), "message": message}
            }),
            "application/json",
        ),
        Protocol::ChatCompletions => (
            serde_json::json!({
                "error": {
                    "message": message,
                    "type": openai_error_type(status),
                    "param": null,
                    "code": openai_error_code(status)
                }
            }),
            "application/json",
        ),
        Protocol::Responses => (
            serde_json::json!({
                "error": {
                    "type": "invalid_request_error",
                    "code": responses_error_code(status),
                    "message": message
                }
            }),
            "application/json",
        ),
    };
    (status, [(axum::http::header::CONTENT_TYPE, content_type)], axum::Json(body)).into_response()
}

pub fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE => "invalid_request_error",
        StatusCode::BAD_GATEWAY => "api_error",
        _ => "api_error",
    }
}

fn openai_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE => "invalid_request_error",
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::BAD_GATEWAY => "upstream_error",
        _ => "server_error",
    }
}

fn openai_error_code(status: StatusCode) -> Option<&'static str> {
    match status {
        StatusCode::BAD_GATEWAY => Some("upstream_error"),
        StatusCode::PAYLOAD_TOO_LARGE => Some("request_too_large"),
        StatusCode::TOO_MANY_REQUESTS => Some("rate_limit_exceeded"),
        _ => None,
    }
}

fn responses_error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE => "invalid_request",
        StatusCode::BAD_GATEWAY => "upstream_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_exceeded",
        StatusCode::UNAUTHORIZED => "authentication_error",
        _ => "server_error",
    }
}

impl AppError {
    pub fn status_code(&self) -> StatusCode {
        error_status(self)
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Storage(value.to_string())
    }
}
impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self::BadRequest(value.to_string())
    }
}
