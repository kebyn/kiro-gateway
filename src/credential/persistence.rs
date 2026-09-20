use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::{auth::Credential, error::AppError};
use uuid::Uuid;

/// Atomically writes the selected credential JSON. Secrets are only written to
/// the explicitly configured path and never logged.
pub fn write_json(path: &Path, credential: &Credential) -> Result<(), AppError> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| AppError::Credential(e.to_string()))?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Credential("credential path must name a file".into()))?;
    let temp = parent.join(format!(".{name}.tmp-{}", Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).map_err(|e| AppError::Credential(e.to_string()))?;
        // SecretString serializes as REDACTED for logs and API output. This is the
        // explicit opt-in persistence path where the actual values are required.
        let mut value = serde_json::json!({
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
        });
        if matches!(credential.auth_method, crate::auth::AuthMethod::ApiKey) {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "api_key".into(),
                    credential
                        .access_token
                        .as_ref()
                        .map(|v| serde_json::Value::String(v.expose_secret().to_owned()))
                        .unwrap_or(serde_json::Value::Null),
                );
                object.remove("access_token");
            }
        }
        let body =
            serde_json::to_vec_pretty(&value).map_err(|e| AppError::Credential(e.to_string()))?;
        file.write_all(&body).map_err(|e| AppError::Credential(e.to_string()))?;
        file.sync_all().map_err(|e| AppError::Credential(e.to_string()))?;
        fs::rename(&temp, path).map_err(|e| AppError::Credential(e.to_string()))?;
        // Persist the directory entry as well so a crash cannot leave the
        // old name pointing at an indeterminate temporary file.
        if let Ok(directory) = OpenOptions::new().read(true).open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
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

    #[cfg(unix)]
    #[test]
    fn persisted_credential_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.json");
        write_json(&path, &Credential::default()).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
