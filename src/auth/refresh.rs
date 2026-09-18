use crate::{
    auth::{Credential, SecretString},
    error::AppError,
};
use chrono::{Duration as ChronoDuration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    expires_at: Option<String>,
}

pub async fn refresh(
    client: &Client,
    credential: &mut Credential,
    timeout: Duration,
) -> Result<(), AppError> {
    let refresh_token = credential
        .refresh_token
        .as_ref()
        .ok_or_else(|| AppError::Credential("refresh token is missing".into()))?
        .expose_secret()
        .to_owned();
    let url = std::env::var("KIRO_TOKEN_ENDPOINT")
        .unwrap_or_else(|_| "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token".into());
    let mut request = client
        .post(url)
        .timeout(timeout)
        .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token.as_str())]);
    if let Some(client_id) = &credential.client_id {
        request = request.form(&[("client_id", client_id.expose_secret())]);
    }
    if let Some(client_secret) = &credential.client_secret {
        request = request.form(&[("client_secret", client_secret.expose_secret())]);
    }
    let response = request
        .send()
        .await
        .map_err(|e| AppError::Upstream(format!("token refresh request failed: {e}")))?;
    if !response.status().is_success() {
        return Err(AppError::Upstream(format!("token refresh returned {}", response.status())));
    }
    let body: RefreshResponse = response
        .json()
        .await
        .map_err(|e| AppError::Upstream(format!("invalid token refresh response: {e}")))?;
    let token = body
        .access_token
        .ok_or_else(|| AppError::Credential("refresh response has no access_token".into()))?;
    credential.access_token = Some(SecretString::new(token));
    if let Some(token) = body.refresh_token {
        credential.refresh_token = Some(SecretString::new(token));
    }
    credential.expires_at = body
        .expires_at
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(&v).ok())
        .map(|v| v.with_timezone(&Utc))
        .or_else(|| body.expires_in.map(|seconds| Utc::now() + ChronoDuration::seconds(seconds)));
    Ok(())
}
