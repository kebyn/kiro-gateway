use super::{AuthMethod, Credential, SecretString};
use crate::config::AppConfig;
use crate::error::AppError;

/// Loads one credential from the v2 `credential` configuration group.
/// Supplying mutually exclusive credential kinds is an error instead of an
/// implicit precedence rule. Environment overrides are applied by
/// `AppConfig::apply_env`, so this module never reads legacy flat variables.
pub fn load(config: &AppConfig) -> Result<Vec<Credential>, AppError> {
    let access = config.credential_access_token.clone().filter(|value| !value.trim().is_empty());
    let refresh = config.credential_refresh_token.clone().filter(|value| !value.trim().is_empty());
    let api_key = config.credential_api_key.clone().filter(|value| !value.trim().is_empty());
    if access.is_none() && refresh.is_none() && api_key.is_none() {
        return Ok(Vec::new());
    }
    if api_key.is_some() && (access.is_some() || refresh.is_some()) {
        return Err(AppError::Credential(
            "credential.api_key cannot be combined with access_token or refresh_token".into(),
        ));
    }
    let auth_method = if api_key.is_some() { AuthMethod::ApiKey } else { AuthMethod::RefreshToken };
    let credential = Credential {
        auth_method,
        access_token: access.or(api_key).map(SecretString::new),
        refresh_token: refresh.map(SecretString::new),
        client_id: config
            .credential_client_id
            .clone()
            .filter(|v| !v.trim().is_empty())
            .map(SecretString::new),
        client_secret: config
            .credential_client_secret
            .clone()
            .filter(|v| !v.trim().is_empty())
            .map(SecretString::new),
        api_region: config.api_region.clone(),
        endpoint: if config.endpoint == "auto" {
            // Environment credentials do not carry endpoint metadata. Keep
            // `upstream.endpoint=auto` usable by selecting the IDE protocol;
            // callers can explicitly choose `cli` when needed.
            "ide".into()
        } else {
            config.endpoint.clone()
        },
        machine_id: config
            .credential_machine_id
            .clone()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        ..Default::default()
    };
    credential.validate_external().map_err(AppError::Credential)?;
    Ok(vec![credential])
}

#[cfg(test)]
mod tests {
    use super::load;
    use crate::config::AppConfig;

    #[test]
    fn loads_v2_credential_fields_and_defaults_endpoint_for_env_credentials() {
        let config = AppConfig {
            credential_access_token: Some("access-token".into()),
            credential_refresh_token: Some("refresh-token".into()),
            credential_client_id: Some("client-id".into()),
            credential_client_secret: Some("client-secret".into()),
            api_region: "eu-central-1".into(),
            ..Default::default()
        };
        let credential = load(&config).unwrap().pop().unwrap();
        assert_eq!(credential.api_region, "eu-central-1");
        assert_eq!(credential.endpoint, "ide");
        assert_eq!(credential.access_token.unwrap().expose_secret(), "access-token");
        assert_eq!(credential.refresh_token.unwrap().expose_secret(), "refresh-token");
    }

    #[test]
    fn rejects_api_key_mixed_with_tokens() {
        let config = AppConfig {
            credential_access_token: Some("access-token".into()),
            credential_api_key: Some("api-key".into()),
            ..Default::default()
        };
        assert!(load(&config).is_err());
    }
}
