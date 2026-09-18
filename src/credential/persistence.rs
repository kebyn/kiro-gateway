use std::{fs, io::Write, path::Path};

use crate::{auth::Credential, error::AppError};

/// Atomically writes the selected credential JSON. Secrets are only written to
/// the explicitly configured path and never logged.
pub fn write_json(path: &Path, credential: &Credential) -> Result<(), AppError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| AppError::Credential(e.to_string()))?;
    let temp = path.with_extension("tmp");
    let mut file = fs::File::create(&temp).map_err(|e| AppError::Credential(e.to_string()))?;
    // SecretString serializes as REDACTED for logs and API output. This is the
    // explicit opt-in persistence path where the actual values are required.
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "auth_method": credential.auth_method,
        "access_token": credential.access_token.as_ref().map(|v| v.expose_secret()),
        "refresh_token": credential.refresh_token.as_ref().map(|v| v.expose_secret()),
        "client_id": credential.client_id.as_ref().map(|v| v.expose_secret()),
        "client_secret": credential.client_secret.as_ref().map(|v| v.expose_secret()),
        "profile_arn": credential.profile_arn,
        "sso_region": credential.sso_region,
        "api_region": credential.api_region,
        "endpoint": credential.endpoint,
        "machine_id": credential.machine_id,
        "expires_at": credential.expires_at,
        "source": credential.source,
    }))
    .map_err(|e| AppError::Credential(e.to_string()))?;
    file.write_all(&body).map_err(|e| AppError::Credential(e.to_string()))?;
    file.sync_all().map_err(|e| AppError::Credential(e.to_string()))?;
    fs::rename(&temp, path).map_err(|e| AppError::Credential(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthMethod, SecretString};

    #[test]
    fn explicit_persistence_keeps_secret_for_refresh_rotation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.json");
        let credential = Credential {
            auth_method: AuthMethod::RefreshToken,
            access_token: Some(SecretString::new("access-secret")),
            refresh_token: Some(SecretString::new("refresh-secret")),
            ..Default::default()
        };
        write_json(&path, &credential).unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("access-secret"));
        assert!(!format!("{credential:?}").contains("access-secret"));
    }
}
