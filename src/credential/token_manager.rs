use std::{sync::Arc, time::Duration};

use chrono::Utc;
use parking_lot::RwLock;
use reqwest::Client;
use tokio::time::{sleep, timeout};

use crate::{
    auth::{self, Credential, CredentialStatus},
    config::AppConfig,
    error::AppError,
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
    persistence_path: Option<std::path::PathBuf>,
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
        Ok(Self {
            credential: Arc::new(RwLock::new(credential)),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            state: Arc::new(RwLock::new(RefreshState::default())),
            client,
            early_secs: config.refresh_early_secs,
            timeout: Duration::from_secs(config.upstream_timeout_secs),
            persistence_path: config
                .credential_json_path
                .as_ref()
                .map(|v| AppConfig::expanded_path(v)),
        })
    }

    pub fn credential(&self) -> Credential {
        self.credential.read().clone()
    }
    pub fn replace_credential(&self, credential: Credential) {
        *self.credential.write() = credential;
    }
    pub fn status(&self) -> CredentialStatus {
        self.credential.read().status(Utc::now(), self.early_secs)
    }
    pub fn refresh_state(&self) -> RefreshState {
        self.state.read().clone()
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
                auth::refresh::refresh(&self.client, &mut credential, self.timeout),
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
                if let Err(error) = manager.ensure_fresh().await {
                    tracing::warn!(error = %error, "background credential refresh failed");
                }
            }
        })
    }

    pub fn bearer_token(&self) -> Option<String> {
        self.credential.read().access_token.as_ref().map(|s| s.expose_secret().to_owned())
    }
}
