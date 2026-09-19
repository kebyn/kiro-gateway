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
    let kind = config.credential_source.to_ascii_lowercase();
    let mut candidates = Vec::new();
    let load_json = |path: &Path| {
        json::load(path).map(|items| {
            items
                .into_iter()
                .map(|mut c| {
                    c.source = Some(path.display().to_string());
                    CredentialCandidate { credential: c }
                })
                .collect::<Vec<_>>()
        })
    };
    match kind.as_str() {
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
        "env" => candidates.extend(env::load().into_iter().map(|mut c| {
            c.source = Some("environment".into());
            CredentialCandidate { credential: c }
        })),
        "api_key" | "apikey" => {
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
        _ => {
            candidates.extend(env::load().into_iter().map(|mut c| {
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
    }
    if candidates.is_empty() {
        return Err(AppError::Credential(
            "no credential source produced a usable credential".into(),
        ));
    }
    if candidates.len() > 1 && kind == "auto" {
        return Err(AppError::Credential(
            "multiple credentials discovered; choose credential_source explicitly".into(),
        ));
    }
    Ok(candidates)
}
