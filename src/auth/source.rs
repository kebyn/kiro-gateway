use std::path::Path;

use crate::{
    auth::{env, json, sqlite},
    config::AppConfig,
    error::AppError,
};

#[derive(Clone, Debug)]
pub struct CredentialCandidate {
    pub credential: super::Credential,
}

pub fn discover(config: &AppConfig) -> Result<Vec<CredentialCandidate>, AppError> {
    let kind = config.credential_source.as_str();
    let mut candidates = Vec::new();
    let load_json = |path: &Path| {
        json::load(path).map(|mut credential| {
            credential.source = Some(path.display().to_string());
            vec![CredentialCandidate { credential }]
        })
    };
    match kind {
        "json" => {
            let path = config
                .credential_json_path
                .as_deref()
                .ok_or_else(|| AppError::Credential("credential_json_path is required".into()))?;
            candidates.extend(load_json(&AppConfig::expanded_path(path))?);
        }
        "sqlite" => {
            let path = config
                .credential_path
                .as_deref()
                .ok_or_else(|| AppError::Credential("credential_path is required".into()))?;
            candidates.extend(sqlite::load(&AppConfig::expanded_path(path))?.into_iter().map(
                |mut c| {
                    c.source = Some(path.to_owned());
                    CredentialCandidate { credential: c }
                },
            ));
        }
        "env" => candidates.extend(env::load()?.into_iter().map(|mut c| {
            c.source = Some("environment".into());
            CredentialCandidate { credential: c }
        })),
        "api_key" => {
            let key = std::env::var("KIRO_API_KEY")
                .map_err(|_| AppError::Credential("KIRO_API_KEY is not set".into()))?;
            candidates.push(CredentialCandidate {
                credential: super::Credential {
                    auth_method: super::AuthMethod::ApiKey,
                    access_token: Some(super::SecretString::new(key)),
                    api_region: config.api_region.clone(),
                    endpoint: config.endpoint.clone(),
                    machine_id: uuid::Uuid::new_v4().to_string(),
                    ..Default::default()
                },
            });
        }
        "auto" => {
            candidates.extend(env::load()?.into_iter().map(|mut c| {
                c.source = Some("environment".into());
                CredentialCandidate { credential: c }
            }));
            if let Some(path) = config.credential_json_path.as_deref() {
                let path = AppConfig::expanded_path(path);
                if path.exists() {
                    candidates.extend(load_json(&path)?);
                }
            }
            let sqlite_path =
                config.credential_path.as_deref().map(AppConfig::expanded_path).or_else(|| {
                    std::env::var("HOME").ok().map(|home| {
                        std::path::PathBuf::from(home).join(".local/share/kiro-cli/data.sqlite3")
                    })
                });
            if let Some(path) = sqlite_path {
                if path.exists() {
                    candidates.extend(sqlite::load(&path)?.into_iter().map(|mut c| {
                        c.source = Some(path.display().to_string());
                        CredentialCandidate { credential: c }
                    }));
                }
            }
        }
        _ => unreachable!("AppConfig::validate rejects unknown credential source"),
    }
    if candidates.is_empty() {
        return Err(AppError::Credential(
            "no credential source produced a usable credential".into(),
        ));
    }
    if candidates.len() > 1 {
        return Err(AppError::Credential(
            "multiple credentials discovered; configure exactly one credential".into(),
        ));
    }
    candidates[0].credential.validate_external().map_err(AppError::Credential)?;
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::discover;
    use crate::config::AppConfig;
    use std::fs;

    #[test]
    fn explicit_json_source_rejects_multiple_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        fs::write(
            &path,
            r#"[
                {"access_token":"first"},
                {"access_token":"second"}
            ]"#,
        )
        .unwrap();
        let config = AppConfig {
            credential_source: "json".into(),
            credential_json_path: Some(path.display().to_string()),
            ..Default::default()
        };

        let error = discover(&config).expect_err("single-tenant source must reject arrays");
        assert!(error.to_string().contains("credential JSON"));
    }
}
