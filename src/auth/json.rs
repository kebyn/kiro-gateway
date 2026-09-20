use super::{AuthMethod, Credential, SecretString};
use crate::error::AppError;
use serde_json::Value;
use std::{fs, path::Path};

pub fn load(path: &Path) -> Result<Vec<Credential>, AppError> {
    let value: Value = serde_json::from_str(
        &fs::read_to_string(path)
            .map_err(|e| AppError::Credential(format!("{}: {e}", path.display())))?,
    )?;
    let values = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    values.into_iter().map(parse).collect()
}

fn parse(value: Value) -> Result<Credential, AppError> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::Credential("credential JSON must be an object".into()))?;
    let string = |names: &[&str]| {
        names.iter().find_map(|n| {
            object.get(*n).and_then(Value::as_str).filter(|v| !v.is_empty()).map(ToOwned::to_owned)
        })
    };
    let api_key = string(&["api_key", "apiKey", "kiro_api_key", "kiroApiKey"]);
    let auth_method =
        string(&["auth_method", "authMethod", "type"]).and_then(|v| v.parse().ok()).unwrap_or_else(
            || {
                if api_key.is_some() { AuthMethod::ApiKey } else { AuthMethod::Unknown }
            },
        );
    let expires_at = string(&["expires_at", "expiresAt"])
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(&v).ok())
        .map(|v| v.with_timezone(&chrono::Utc));
    Ok(Credential {
        auth_method,
        access_token: string(&["access_token", "accessToken", "token"])
            .or(api_key)
            .map(SecretString::new),
        refresh_token: string(&["refresh_token", "refreshToken"]).map(SecretString::new),
        client_id: string(&["client_id", "clientId"]).map(SecretString::new),
        client_secret: string(&["client_secret", "clientSecret"]).map(SecretString::new),
        profile_arn: string(&["profile_arn", "profileArn"]),
        sso_region: string(&["sso_region", "ssoRegion"]),
        api_region: string(&["api_region", "apiRegion", "region"])
            .unwrap_or_else(|| "us-east-1".into()),
        endpoint: string(&["endpoint"]).unwrap_or_else(|| "ide".into()),
        machine_id: string(&["machine_id", "machineId"])
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        expires_at,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::auth::AuthMethod;
    use serde_json::json;

    #[test]
    fn loads_api_key_aliases_as_the_access_token() {
        for key in ["api_key", "apiKey", "kiro_api_key", "kiroApiKey"] {
            let credential = parse(json!({key: "upstream-secret"})).unwrap();
            assert_eq!(credential.auth_method, AuthMethod::ApiKey);
            assert_eq!(
                credential.access_token.as_ref().map(|value| value.expose_secret()),
                Some("upstream-secret")
            );
        }
    }
}
