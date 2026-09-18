use crate::{
    auth::{AuthMethod, Credential, SecretString},
    error::AppError,
};
use chrono::{Duration as ChronoDuration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    #[serde(alias = "accessToken")]
    access_token: Option<String>,
    #[serde(alias = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(alias = "expiresIn")]
    expires_in: Option<i64>,
    #[serde(alias = "expiresAt")]
    expires_at: Option<String>,
    #[serde(alias = "profileArn")]
    profile_arn: Option<String>,
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
    let oidc = matches!(credential.auth_method, AuthMethod::Sso)
        || (matches!(credential.auth_method, AuthMethod::Unknown)
            && credential.client_id.is_some());
    let region = credential.sso_region.as_deref().unwrap_or(&credential.api_region);
    let default_url = if oidc {
        format!("https://oidc.{region}.amazonaws.com/token")
    } else {
        format!("https://prod.{region}.auth.desktop.kiro.dev/refreshToken")
    };
    let url = std::env::var("KIRO_TOKEN_ENDPOINT").unwrap_or(default_url);
    let request = if oidc {
        let mut payload =
            serde_json::json!({"grantType":"refresh_token","refreshToken":refresh_token});
        if let Some(client_id) = &credential.client_id {
            payload["clientId"] = serde_json::Value::String(client_id.expose_secret().to_owned());
        }
        if let Some(client_secret) = &credential.client_secret {
            payload["clientSecret"] =
                serde_json::Value::String(client_secret.expose_secret().to_owned());
        }
        client.post(url).timeout(timeout).json(&payload)
    } else {
        client.post(url).timeout(timeout).json(&serde_json::json!({"refreshToken":refresh_token}))
    };
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
    if let Some(profile_arn) = body.profile_arn {
        credential.profile_arn = Some(profile_arn);
    }
    credential.expires_at = body
        .expires_at
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(&v).ok())
        .map(|v| v.with_timezone(&Utc))
        .or_else(|| body.expires_in.map(|seconds| Utc::now() + ChronoDuration::seconds(seconds)));
    Ok(())
}
