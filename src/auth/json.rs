use super::{AuthMethod, Credential, SecretString};
use crate::error::AppError;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::{fs, path::Path};

/// The on-disk credential contract is deliberately narrower than the
/// internal `Credential` type. CamelCase aliases and generic `token` fields
/// are not accepted here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    #[serde(default)]
    auth_method: Option<AuthMethod>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    profile_arn: Option<String>,
    #[serde(default)]
    sso_region: Option<String>,
    #[serde(default = "default_api_region")]
    api_region: String,
    #[serde(default = "default_endpoint")]
    endpoint: String,
    #[serde(default)]
    machine_id: Option<String>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    source: Option<String>,
}

fn default_api_region() -> String {
    "us-east-1".into()
}
fn default_endpoint() -> String {
    "ide".into()
}

pub fn load(path: &Path) -> Result<Credential, AppError> {
    let text = fs::read_to_string(path)
        .map_err(|e| AppError::Credential(format!("{}: {e}", path.display())))?;
    let value: CredentialFile = serde_json::from_str(&text).map_err(|error| {
        AppError::Credential(format!("invalid credential JSON {}: {error}", path.display()))
    })?;
    parse(value)
}

fn parse(value: CredentialFile) -> Result<Credential, AppError> {
    let access_token = value.access_token.filter(|v| !v.is_empty());
    let api_key = value.api_key.filter(|v| !v.is_empty());
    if access_token.is_some() && api_key.is_some() {
        return Err(AppError::Credential(
            "credential JSON must contain either access_token or api_key, not both".into(),
        ));
    }
    let auth_method = match (value.auth_method, api_key.is_some()) {
        (Some(method), _) => method,
        (None, true) => AuthMethod::ApiKey,
        (None, false) if access_token.is_some() || value.refresh_token.is_some() => {
            AuthMethod::RefreshToken
        }
        (None, false) => {
            return Err(AppError::Credential(
                "credential JSON must specify auth_method or a credential token".into(),
            ));
        }
    };
    if api_key.is_some() && !matches!(auth_method, AuthMethod::ApiKey) {
        return Err(AppError::Credential("api_key credential must use auth_method api_key".into()));
    }
    if matches!(auth_method, AuthMethod::ApiKey)
        && value.refresh_token.as_ref().is_some_and(|v| !v.is_empty())
    {
        return Err(AppError::Credential("api_key credential cannot include refresh_token".into()));
    }
    let credential = Credential {
        auth_method,
        access_token: api_key.or(access_token).map(SecretString::new),
        refresh_token: value.refresh_token.filter(|v| !v.is_empty()).map(SecretString::new),
        client_id: value.client_id.filter(|v| !v.is_empty()).map(SecretString::new),
        client_secret: value.client_secret.filter(|v| !v.is_empty()).map(SecretString::new),
        profile_arn: value.profile_arn.filter(|v| !v.is_empty()),
        sso_region: value.sso_region.filter(|v| !v.is_empty()),
        api_region: value.api_region,
        endpoint: value.endpoint,
        machine_id: value
            .machine_id
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        expires_at: value.expires_at,
        source: value.source,
    };
    credential.validate_external().map_err(AppError::Credential)?;
    Ok(credential)
}

#[cfg(test)]
mod tests {
    use super::load;
    use crate::auth::AuthMethod;
    use std::fs;

    fn file(body: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), body).unwrap();
        file
    }

    #[test]
    fn loads_only_canonical_api_key_field() {
        let file = file(r#"{"api_key":"upstream-secret"}"#);
        let credential = load(file.path()).unwrap();
        assert_eq!(credential.auth_method, AuthMethod::ApiKey);
        assert_eq!(credential.access_token.as_ref().unwrap().expose_secret(), "upstream-secret");
    }

    #[test]
    fn rejects_legacy_fields_and_arrays() {
        for body in [
            r#"{"apiKey":"secret"}"#,
            r#"{"token":"secret"}"#,
            r#"{"accessToken":"secret"}"#,
            r#"[{"access_token":"one"},{"access_token":"two"}]"#,
        ] {
            let file = file(body);
            assert!(load(file.path()).is_err(), "legacy shape unexpectedly accepted: {body}");
        }
    }

    #[test]
    fn rejects_unknown_auth_method_spelling() {
        let legacy = file(r#"{"auth_method":"odic","refresh_token":"secret"}"#);
        assert!(load(legacy.path()).is_err());
        let canonical = file(r#"{"auth_method":"oidc","refresh_token":"secret"}"#);
        assert_eq!(load(canonical.path()).unwrap().auth_method, AuthMethod::Oidc);
    }
}
