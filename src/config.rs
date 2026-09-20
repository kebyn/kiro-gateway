use std::{
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::error::AppError;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct AdminConfig {
    pub enabled: bool,
    pub session_ttl_secs: u64,
    pub cookie_secure: bool,
    pub allowed_origins: Vec<String>,
    pub login_rate_limit_per_minute: u32,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            session_ttl_secs: 8 * 60 * 60,
            cookie_secure: true,
            allowed_origins: Vec::new(),
            login_rate_limit_per_minute: 10,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub client_api_key: String,
    pub admin_api_key: String,
    pub admin: AdminConfig,
    pub credential_source: String,
    pub credential_path: Option<String>,
    pub credential_json_path: Option<String>,
    pub endpoint: String,
    pub api_region: String,
    pub mcp_region: Option<String>,
    pub upstream_url: Option<String>,
    pub proxy_url: Option<String>,
    pub token_endpoint: Option<String>,
    pub upstream_timeout_secs: u64,
    pub refresh_early_secs: i64,
    pub refresh_interval_secs: u64,
    pub model_cache_ttl_secs: u64,
    pub response_store_path: String,
    pub max_request_body_bytes: usize,
    pub max_upstream_body_bytes: usize,
    pub graceful_shutdown_timeout_secs: u64,
    pub log_json: bool,
    pub trust_forwarded_headers: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 8990,
            client_api_key: String::new(),
            admin_api_key: String::new(),
            admin: AdminConfig::default(),
            credential_source: "auto".to_owned(),
            credential_path: None,
            credential_json_path: None,
            endpoint: "auto".to_owned(),
            api_region: "us-east-1".to_owned(),
            mcp_region: None,
            upstream_url: None,
            proxy_url: None,
            token_endpoint: None,
            upstream_timeout_secs: 60,
            refresh_early_secs: 120,
            refresh_interval_secs: 30,
            model_cache_ttl_secs: 300,
            response_store_path: "kiro-gateway.sqlite3".to_owned(),
            max_request_body_bytes: 8 * 1024 * 1024,
            max_upstream_body_bytes: 16 * 1024 * 1024,
            graceful_shutdown_timeout_secs: 30,
            log_json: false,
            trust_forwarded_headers: false,
        }
    }
}

impl AppConfig {
    pub fn from_env_and_optional_file(path: Option<&Path>) -> Result<Self, AppError> {
        let mut config = match path {
            Some(path) => {
                let text = fs::read_to_string(path).map_err(|error| {
                    AppError::Config(format!("cannot read {}: {error}", path.display()))
                })?;
                serde_json::from_str(&text)
                    .map_err(|error| AppError::Config(format!("invalid config JSON: {error}")))?
            }
            None => Self::default(),
        };
        config.apply_env()?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self) -> Result<(), AppError> {
        if let Some(v) = env_string("KIRO_CLIENT_API_KEY") {
            self.client_api_key = v;
        }
        if let Some(v) = env_string("KIRO_ADMIN_API_KEY") {
            self.admin_api_key = v;
        }
        if let Some(v) = env_string("KIRO_HOST") {
            self.host = v;
        }
        if let Some(v) = env_parse("KIRO_PORT")? {
            self.port = v;
        }
        if let Some(v) = env_string("KIRO_ENDPOINT") {
            self.endpoint = v;
        }
        if let Some(v) = env_string("KIRO_API_REGION") {
            self.api_region = v;
        }
        if let Some(v) = env_string("KIRO_CREDENTIAL_SOURCE") {
            self.credential_source = v;
        }
        if let Some(v) = env_string("KIRO_CREDENTIAL_PATH") {
            self.credential_path = Some(v);
        }
        if let Some(v) = env_string("KIRO_CREDENTIAL_JSON_PATH") {
            self.credential_json_path = Some(v);
        }
        if let Some(v) = env_string("KIRO_RESPONSE_STORE_PATH") {
            self.response_store_path = v;
        }
        if let Some(v) = env_string("KIRO_UPSTREAM_URL") {
            self.upstream_url = Some(v);
        }
        if let Some(v) = env_string("KIRO_PROXY_URL") {
            self.proxy_url = Some(v);
        }
        if let Some(v) = env_string("KIRO_TOKEN_ENDPOINT") {
            self.token_endpoint = Some(v);
        }
        if let Some(v) = env_parse("KIRO_UPSTREAM_TIMEOUT_SECS")? {
            self.upstream_timeout_secs = v;
        }
        if let Some(v) = env_parse("KIRO_REFRESH_EARLY_SECS")? {
            self.refresh_early_secs = v;
        }
        if let Some(v) = env_parse("KIRO_REFRESH_INTERVAL_SECS")? {
            self.refresh_interval_secs = v;
        }
        if let Some(v) = env_parse("KIRO_MODEL_CACHE_TTL_SECS")? {
            self.model_cache_ttl_secs = v;
        }
        if let Some(v) = env_parse("KIRO_MAX_REQUEST_BODY_BYTES")? {
            self.max_request_body_bytes = v;
        }
        if let Some(v) = env_parse("KIRO_MAX_UPSTREAM_BODY_BYTES")? {
            self.max_upstream_body_bytes = v;
        }
        if let Some(v) = env_parse("KIRO_GRACEFUL_SHUTDOWN_TIMEOUT_SECS")? {
            self.graceful_shutdown_timeout_secs = v;
        }
        if let Some(v) = env_bool("KIRO_ADMIN_ENABLED")? {
            self.admin.enabled = v;
        }
        if let Some(v) = env_parse("KIRO_ADMIN_SESSION_TTL_SECS")? {
            self.admin.session_ttl_secs = v;
        }
        if let Some(v) = env_bool("KIRO_ADMIN_COOKIE_SECURE")? {
            self.admin.cookie_secure = v;
        }
        if let Some(v) = env_parse("KIRO_ADMIN_LOGIN_RATE_LIMIT_PER_MINUTE")? {
            self.admin.login_rate_limit_per_minute = v;
        }
        if let Some(v) = env_string("KIRO_MCP_REGION") {
            self.mcp_region = Some(v);
        }
        if let Some(v) = env_bool("KIRO_LOG_JSON")? {
            self.log_json = v;
        }
        if let Some(v) = env_bool("KIRO_TRUST_FORWARDED_HEADERS")? {
            self.trust_forwarded_headers = v;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.port == 0 {
            return Err(AppError::Config("port must be non-zero".into()));
        }
        if self.client_api_key.trim().is_empty() {
            return Err(AppError::Config("client_api_key is required".into()));
        }
        if self.admin.enabled && self.admin_api_key.trim().is_empty() {
            return Err(AppError::Config("admin_api_key is required when admin is enabled".into()));
        }
        if self.admin.session_ttl_secs == 0 {
            return Err(AppError::Config("admin.session_ttl_secs must be positive".into()));
        }
        if self.admin.login_rate_limit_per_minute == 0 {
            return Err(AppError::Config(
                "admin.login_rate_limit_per_minute must be positive".into(),
            ));
        }
        if self.upstream_timeout_secs == 0 || self.refresh_interval_secs == 0 {
            return Err(AppError::Config("timeouts must be positive".into()));
        }
        if self.max_request_body_bytes == 0 || self.max_upstream_body_bytes == 0 {
            return Err(AppError::Config("body size limits must be positive".into()));
        }
        if self.graceful_shutdown_timeout_secs == 0 {
            return Err(AppError::Config("graceful_shutdown_timeout_secs must be positive".into()));
        }
        if self.model_cache_ttl_secs == 0 {
            return Err(AppError::Config("model_cache_ttl_secs must be positive".into()));
        }
        if self.refresh_early_secs < 0 {
            return Err(AppError::Config("refresh_early_secs must not be negative".into()));
        }
        if !matches!(
            self.credential_source.as_str(),
            "auto" | "env" | "json" | "sqlite" | "api_key"
        ) {
            return Err(AppError::Config(format!(
                "unsupported credential_source: {}",
                self.credential_source
            )));
        }
        if !matches!(self.endpoint.as_str(), "auto" | "ide" | "cli") {
            return Err(AppError::Config(format!("unsupported endpoint: {}", self.endpoint)));
        }
        for (name, value) in [
            ("upstream_url", self.upstream_url.as_deref()),
            ("proxy_url", self.proxy_url.as_deref()),
            ("token_endpoint", self.token_endpoint.as_deref()),
        ] {
            if let Some(value) = value {
                let parsed = url::Url::parse(value)
                    .map_err(|error| AppError::Config(format!("{name} is invalid: {error}")))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(AppError::Config(format!("{name} must use http or https")));
                }
            }
        }
        Ok(())
    }

    pub fn expanded_path(value: &str) -> PathBuf {
        if let Some(rest) = value.strip_prefix("~/") {
            if let Ok(home) = env::var("HOME") {
                return PathBuf::from(home).join(rest);
            }
        }
        PathBuf::from(value)
    }
}

fn env_string(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_parse<T>(name: &str) -> Result<Option<T>, AppError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value
            .parse::<T>()
            .map(Some)
            .map_err(|error| AppError::Config(format!("{name} is invalid: {error}"))),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(AppError::Config(format!("cannot read {name}: {error}"))),
    }
}

fn env_bool(name: &str) -> Result<Option<bool>, AppError> {
    match env::var(name) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => Err(AppError::Config(format!("{name} must be a boolean"))),
        },
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(AppError::Config(format!("cannot read {name}: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{AdminConfig, AppConfig};

    #[test]
    fn canonical_fields_are_accepted() {
        let config: AppConfig = serde_json::from_str(
            r#"{"client_api_key":"client","admin_api_key":"admin","endpoint":"cli"}"#,
        )
        .unwrap();
        assert_eq!(config.client_api_key, "client");
        assert_eq!(config.endpoint, "cli");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn legacy_aliases_are_rejected() {
        for value in [
            r#"{"apiKey":"client","admin_api_key":"admin"}"#,
            r#"{"client_api_key":"client","adminApiKey":"admin"}"#,
            r#"{"client_api_key":"client","defaultEndpoint":"cli"}"#,
        ] {
            assert!(serde_json::from_str::<AppConfig>(value).is_err());
        }
    }

    #[test]
    fn rejects_unknown_source_endpoint_and_negative_refresh_window() {
        let mut config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            ..Default::default()
        };
        config.credential_source = "vault".into();
        assert!(config.validate().is_err());
        config.credential_source = "auto".into();
        config.endpoint = "unknown".into();
        assert!(config.validate().is_err());
        config.endpoint = "ide".into();
        config.refresh_early_secs = -1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn admin_can_be_disabled_without_an_admin_key() {
        let config = AppConfig {
            client_api_key: "client".into(),
            admin: AdminConfig { enabled: false, ..Default::default() },
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn defaults_to_credential_aware_endpoint_selection() {
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            ..Default::default()
        };
        assert_eq!(config.endpoint, "auto");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn token_endpoint_and_body_limits_are_validated() {
        let mut config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            token_endpoint: Some("https://oidc.example.test/token".into()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        config.token_endpoint = Some("ftp://example.test/token".into());
        assert!(config.validate().is_err());
        config.token_endpoint = None;
        config.max_request_body_bytes = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_explicit_missing_config_path_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.json");
        assert!(AppConfig::from_env_and_optional_file(Some(&path)).is_err());
    }
}
