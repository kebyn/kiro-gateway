use super::{AuthMethod, Credential, SecretString};
use crate::error::AppError;

/// Loads one credential from the explicitly named environment variables.
/// Supplying mutually exclusive credential kinds is an error instead of an
/// implicit precedence rule.
pub fn load() -> Result<Vec<Credential>, AppError> {
    let access = non_empty("KIRO_ACCESS_TOKEN");
    let refresh = non_empty("KIRO_REFRESH_TOKEN");
    let api_key = non_empty("KIRO_API_KEY");
    if access.is_none() && refresh.is_none() && api_key.is_none() {
        return Ok(Vec::new());
    }
    if api_key.is_some() && (access.is_some() || refresh.is_some()) {
        return Err(AppError::Credential(
            "KIRO_API_KEY cannot be combined with access or refresh token variables".into(),
        ));
    }
    let auth_method = if api_key.is_some() { AuthMethod::ApiKey } else { AuthMethod::RefreshToken };
    let credential = Credential {
        auth_method,
        access_token: access.or(api_key).map(SecretString::new),
        refresh_token: refresh.map(SecretString::new),
        client_id: non_empty("KIRO_CLIENT_ID").map(SecretString::new),
        client_secret: non_empty("KIRO_CLIENT_SECRET").map(SecretString::new),
        api_region: std::env::var("KIRO_API_REGION").unwrap_or_else(|_| "us-east-1".into()),
        endpoint: std::env::var("KIRO_ENDPOINT").unwrap_or_else(|_| "auto".into()),
        machine_id: std::env::var("KIRO_MACHINE_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        ..Default::default()
    };
    credential.validate_external().map_err(AppError::Credential)?;
    Ok(vec![credential])
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}
