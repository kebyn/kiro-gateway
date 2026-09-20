use std::{sync::Arc, time::Duration};

use chrono::Utc;
use parking_lot::RwLock;
use reqwest::Client;
use tokio::time::{sleep, timeout};

use crate::{
    auth::{self, Credential, CredentialStatus},
    config::AppConfig,
    error::AppError,
    model_catalog::{ModelCatalog, ModelInfo},
};

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RefreshState {
    pub last_attempt: Option<chrono::DateTime<Utc>>,
    pub last_success: Option<chrono::DateTime<Utc>>,
    pub last_error: Option<String>,
    pub refreshing: bool,
}

#[derive(Clone)]
pub struct TokenManager {
    credential: Arc<RwLock<Credential>>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    state: Arc<RwLock<RefreshState>>,
    client: Client,
    early_secs: i64,
    timeout: Duration,
    token_endpoint: Option<String>,
    persistence_path: Option<std::path::PathBuf>,
    model_catalog: Arc<ModelCatalog>,
}

impl TokenManager {
    pub fn new(config: &AppConfig, credential: Credential) -> Result<Self, AppError> {
        let mut builder =
            Client::builder().timeout(Duration::from_secs(config.upstream_timeout_secs));
        if let Some(proxy) = &config.proxy_url {
            builder = builder
                .proxy(reqwest::Proxy::all(proxy).map_err(|e| AppError::Config(e.to_string()))?);
        }
        let client = builder.build().map_err(|e| AppError::Config(e.to_string()))?;
        let model_catalog = Arc::new(ModelCatalog::new_with_limit(
            client.clone(),
            Duration::from_secs(config.model_cache_ttl_secs),
            config.max_upstream_body_bytes,
        ));
        Ok(Self {
            credential: Arc::new(RwLock::new(credential)),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            state: Arc::new(RwLock::new(RefreshState::default())),
            client,
            early_secs: config.refresh_early_secs,
            timeout: Duration::from_secs(config.upstream_timeout_secs),
            token_endpoint: config.token_endpoint.clone(),
            persistence_path: config
                .credential_json_path
                .as_ref()
                .map(|v| AppConfig::expanded_path(v)),
            model_catalog,
        })
    }

    pub fn credential(&self) -> Credential {
        self.credential.read().clone()
    }
    pub fn replace_credential(&self, credential: Credential) {
        *self.credential.write() = credential;
        self.model_catalog.invalidate();
    }
    pub fn status(&self) -> CredentialStatus {
        self.credential.read().status(Utc::now(), self.early_secs)
    }
    pub fn refresh_state(&self) -> RefreshState {
        self.state.read().clone()
    }

    pub async fn available_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        self.ensure_fresh().await?;
        self.model_catalog.get(&self.credential()).await
    }

    pub async fn validate_model(&self, model: &str) -> Result<(), AppError> {
        let models = self.available_models().await.map_err(|_error| {
            tracing::warn!(
                error_class = "model_discovery",
                "model discovery failed while validating request"
            );
            AppError::BadRequest("no models are currently available".into())
        })?;
        if models.iter().any(|candidate| candidate.model_id == model) {
            Ok(())
        } else {
            Err(AppError::BadRequest(format!("model is not available: {model}")))
        }
    }

    #[cfg(test)]
    pub fn seed_models_for_tests(&self, models: Vec<ModelInfo>) {
        self.model_catalog.seed(models);
    }

    pub async fn ensure_fresh(&self) -> Result<(), AppError> {
        let needs = { self.credential.read().needs_refresh(Utc::now(), self.early_secs) };
        if !needs {
            return Ok(());
        }
        self.refresh().await
    }

    pub async fn refresh(&self) -> Result<(), AppError> {
        let _guard = self.refresh_lock.lock().await;
        // Another waiter may have refreshed while we waited for the single-flight lock.
        if !self.credential.read().needs_refresh(Utc::now(), self.early_secs) {
            return Ok(());
        }
        self.state.write().refreshing = true;
        self.state.write().last_attempt = Some(Utc::now());
        let result = if matches!(self.credential.read().auth_method, auth::AuthMethod::ApiKey) {
            Ok(())
        } else {
            let mut credential = self.credential();
            let result = timeout(
                self.timeout,
                auth::refresh::refresh(
                    &self.client,
                    &mut credential,
                    self.timeout,
                    self.token_endpoint.as_deref(),
                ),
            )
            .await
            .map_err(|_| AppError::Upstream("token refresh timed out".into()))?;
            if result.is_ok() {
                self.replace_credential(credential);
            }
            result.map(|_| ())
        };
        match result {
            Ok(()) => {
                self.state.write().last_success = Some(Utc::now());
                self.state.write().last_error = None;
                if let Some(path) = &self.persistence_path {
                    let credential = self.credential();
                    if let Err(error) =
                        crate::credential::persistence::write_json(path, &credential)
                    {
                        self.state.write().last_error = Some(error.to_string());
                        self.state.write().refreshing = false;
                        return Err(error);
                    }
                }
                self.state.write().refreshing = false;
                Ok(())
            }
            Err(error) => {
                self.state.write().last_error = Some(error.to_string());
                self.state.write().refreshing = false;
                Err(error)
            }
        }
    }

    pub fn spawn_refresh_task(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(interval).await;
                if let Err(_error) = manager.ensure_fresh().await {
                    tracing::warn!(
                        error_class = "credential_refresh",
                        "background credential refresh failed"
                    );
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TokenManager;
    use crate::{
        auth::{AuthMethod, Credential},
        config::AppConfig,
        model_catalog::ModelInfo,
    };

    #[test]
    fn replacing_credential_invalidates_cached_models() {
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            ..Default::default()
        };
        let manager = TokenManager::new(
            &config,
            Credential { auth_method: AuthMethod::ApiKey, ..Default::default() },
        )
        .unwrap();
        manager.seed_models_for_tests(vec![ModelInfo {
            model_id: "old-model".into(),
            model_name: None,
            description: None,
            token_limits: None,
        }]);
        assert!(manager.model_catalog.has_cached_result());

        manager.replace_credential(Credential::default());

        assert!(!manager.model_catalog.has_cached_result());
    }
}
