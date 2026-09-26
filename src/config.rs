use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use rand::{TryRngCore, rngs::OsRng};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::AppError;

pub const CONFIG_VERSION: u8 = 2;

#[derive(Clone, Debug)]
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
            enabled: false,
            session_ttl_secs: 8 * 60 * 60,
            cookie_secure: true,
            allowed_origins: Vec::new(),
            login_rate_limit_per_minute: 10,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub version: u8,
    // Internal accessors remain flat so protocol/application code does not
    // need to know the wire-format grouping.
    pub host: String,
    pub port: u16,
    pub client_api_key: String,
    pub admin_api_key: String,
    pub admin: AdminConfig,
    pub credential_source: String,
    pub credential_path: Option<String>,
    pub credential_json_path: Option<String>,
    pub credential_access_token: Option<String>,
    pub credential_refresh_token: Option<String>,
    pub credential_api_key: Option<String>,
    pub credential_client_id: Option<String>,
    pub credential_client_secret: Option<String>,
    pub credential_machine_id: Option<String>,
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
    pub model_aliases: HashMap<String, String>,
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
            version: CONFIG_VERSION,
            host: "127.0.0.1".into(),
            port: 8990,
            client_api_key: String::new(),
            admin_api_key: String::new(),
            admin: AdminConfig::default(),
            credential_source: "auto".into(),
            credential_path: None,
            credential_json_path: None,
            credential_access_token: None,
            credential_refresh_token: None,
            credential_api_key: None,
            credential_client_id: None,
            credential_client_secret: None,
            credential_machine_id: None,
            endpoint: "auto".into(),
            api_region: "us-east-1".into(),
            mcp_region: None,
            upstream_url: None,
            proxy_url: None,
            token_endpoint: None,
            upstream_timeout_secs: 60,
            refresh_early_secs: 120,
            refresh_interval_secs: 30,
            model_cache_ttl_secs: 300,
            model_aliases: HashMap::new(),
            response_store_path: "kiro-gateway.sqlite3".into(),
            max_request_body_bytes: 8 * 1024 * 1024,
            max_upstream_body_bytes: 16 * 1024 * 1024,
            graceful_shutdown_timeout_secs: 30,
            log_json: false,
            trust_forwarded_headers: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct ServerWire {
    host: Option<String>,
    port: Option<u16>,
    max_request_body_bytes: Option<usize>,
    graceful_shutdown_timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct AccessWire {
    client_api_key: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct AdminWire {
    enabled: Option<bool>,
    api_key: Option<String>,
    session_ttl_secs: Option<u64>,
    cookie_secure: Option<bool>,
    allowed_origins: Option<Vec<String>>,
    login_rate_limit_per_minute: Option<u32>,
    trust_forwarded_headers: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct CredentialWire {
    source: Option<String>,
    path: Option<String>,
    json_path: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    api_key: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    machine_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct UpstreamWire {
    endpoint: Option<String>,
    api_region: Option<String>,
    url: Option<String>,
    proxy_url: Option<String>,
    token_endpoint: Option<String>,
    timeout_secs: Option<u64>,
    max_body_bytes: Option<usize>,
    refresh_early_secs: Option<i64>,
    refresh_interval_secs: Option<u64>,
    mcp_region: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct ModelsWire {
    cache_ttl_secs: Option<u64>,
    aliases: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct StorageWire {
    response_store_path: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct LoggingWire {
    format: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigWire {
    version: u8,
    #[serde(default)]
    server: ServerWire,
    #[serde(default)]
    access: AccessWire,
    #[serde(default)]
    admin: AdminWire,
    #[serde(default)]
    credential: CredentialWire,
    #[serde(default)]
    upstream: UpstreamWire,
    #[serde(default)]
    models: ModelsWire,
    #[serde(default)]
    storage: StorageWire,
    #[serde(default)]
    logging: LoggingWire,
}

impl From<&AppConfig> for ConfigWire {
    fn from(config: &AppConfig) -> Self {
        Self {
            version: CONFIG_VERSION,
            server: ServerWire {
                host: Some(config.host.clone()),
                port: Some(config.port),
                max_request_body_bytes: Some(config.max_request_body_bytes),
                graceful_shutdown_timeout_secs: Some(config.graceful_shutdown_timeout_secs),
            },
            access: AccessWire { client_api_key: Some(config.client_api_key.clone()) },
            admin: AdminWire {
                enabled: Some(config.admin.enabled),
                api_key: Some(config.admin_api_key.clone()),
                session_ttl_secs: Some(config.admin.session_ttl_secs),
                cookie_secure: Some(config.admin.cookie_secure),
                allowed_origins: Some(config.admin.allowed_origins.clone()),
                login_rate_limit_per_minute: Some(config.admin.login_rate_limit_per_minute),
                trust_forwarded_headers: Some(config.trust_forwarded_headers),
            },
            credential: CredentialWire {
                source: Some(config.credential_source.clone()),
                path: config.credential_path.clone(),
                json_path: config.credential_json_path.clone(),
                access_token: config.credential_access_token.clone(),
                refresh_token: config.credential_refresh_token.clone(),
                api_key: config.credential_api_key.clone(),
                client_id: config.credential_client_id.clone(),
                client_secret: config.credential_client_secret.clone(),
                machine_id: config.credential_machine_id.clone(),
            },
            upstream: UpstreamWire {
                endpoint: Some(config.endpoint.clone()),
                api_region: Some(config.api_region.clone()),
                url: config.upstream_url.clone(),
                proxy_url: config.proxy_url.clone(),
                token_endpoint: config.token_endpoint.clone(),
                timeout_secs: Some(config.upstream_timeout_secs),
                max_body_bytes: Some(config.max_upstream_body_bytes),
                refresh_early_secs: Some(config.refresh_early_secs),
                refresh_interval_secs: Some(config.refresh_interval_secs),
                mcp_region: config.mcp_region.clone(),
            },
            models: ModelsWire {
                cache_ttl_secs: Some(config.model_cache_ttl_secs),
                aliases: Some(config.model_aliases.clone()),
            },
            storage: StorageWire { response_store_path: Some(config.response_store_path.clone()) },
            logging: LoggingWire {
                format: Some(if config.log_json { "json" } else { "text" }.into()),
            },
        }
    }
}

impl TryFrom<ConfigWire> for AppConfig {
    type Error = AppError;

    fn try_from(wire: ConfigWire) -> Result<Self, Self::Error> {
        if wire.version != CONFIG_VERSION {
            return Err(AppError::Config(format!(
                "unsupported configuration version {}; expected {CONFIG_VERSION}",
                wire.version
            )));
        }
        let defaults = Self::default();
        let format = wire.logging.format.as_deref().unwrap_or("text");
        if !matches!(format, "text" | "json") {
            return Err(AppError::Config(format!("unsupported logging.format: {format}")));
        }
        let host = wire.server.host.unwrap_or(defaults.host);
        let source = wire.credential.source.unwrap_or(defaults.credential_source);
        let endpoint = wire.upstream.endpoint.unwrap_or(defaults.endpoint);
        let api_region = wire.upstream.api_region.unwrap_or(defaults.api_region);
        Ok(Self {
            version: CONFIG_VERSION,
            host,
            port: wire.server.port.unwrap_or(defaults.port),
            client_api_key: wire.access.client_api_key.unwrap_or_default(),
            admin_api_key: wire.admin.api_key.unwrap_or_default(),
            admin: AdminConfig {
                enabled: wire.admin.enabled.unwrap_or(defaults.admin.enabled),
                session_ttl_secs: wire
                    .admin
                    .session_ttl_secs
                    .unwrap_or(defaults.admin.session_ttl_secs),
                cookie_secure: wire.admin.cookie_secure.unwrap_or(defaults.admin.cookie_secure),
                allowed_origins: wire
                    .admin
                    .allowed_origins
                    .unwrap_or(defaults.admin.allowed_origins),
                login_rate_limit_per_minute: wire
                    .admin
                    .login_rate_limit_per_minute
                    .unwrap_or(defaults.admin.login_rate_limit_per_minute),
            },
            credential_source: source,
            credential_path: wire.credential.path,
            credential_json_path: wire.credential.json_path,
            credential_access_token: wire.credential.access_token,
            credential_refresh_token: wire.credential.refresh_token,
            credential_api_key: wire.credential.api_key,
            credential_client_id: wire.credential.client_id,
            credential_client_secret: wire.credential.client_secret,
            credential_machine_id: wire.credential.machine_id,
            endpoint,
            api_region,
            mcp_region: wire.upstream.mcp_region,
            upstream_url: wire.upstream.url,
            proxy_url: wire.upstream.proxy_url,
            token_endpoint: wire.upstream.token_endpoint,
            upstream_timeout_secs: wire
                .upstream
                .timeout_secs
                .unwrap_or(defaults.upstream_timeout_secs),
            refresh_early_secs: wire
                .upstream
                .refresh_early_secs
                .unwrap_or(defaults.refresh_early_secs),
            refresh_interval_secs: wire
                .upstream
                .refresh_interval_secs
                .unwrap_or(defaults.refresh_interval_secs),
            model_cache_ttl_secs: wire
                .models
                .cache_ttl_secs
                .unwrap_or(defaults.model_cache_ttl_secs),
            model_aliases: wire.models.aliases.unwrap_or_default(),
            response_store_path: wire
                .storage
                .response_store_path
                .unwrap_or(defaults.response_store_path),
            max_request_body_bytes: wire
                .server
                .max_request_body_bytes
                .unwrap_or(defaults.max_request_body_bytes),
            max_upstream_body_bytes: wire
                .upstream
                .max_body_bytes
                .unwrap_or(defaults.max_upstream_body_bytes),
            graceful_shutdown_timeout_secs: wire
                .server
                .graceful_shutdown_timeout_secs
                .unwrap_or(defaults.graceful_shutdown_timeout_secs),
            log_json: format == "json",
            trust_forwarded_headers: wire
                .admin
                .trust_forwarded_headers
                .unwrap_or(defaults.trust_forwarded_headers),
        })
    }
}

impl Serialize for AppConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ConfigWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AppConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ConfigWire::deserialize(deserializer)
            .and_then(|wire| Self::try_from(wire).map_err(serde::de::Error::custom))
    }
}

impl AppConfig {
    pub fn generated_template() -> Result<Self, AppError> {
        Ok(Self {
            client_api_key: random_api_key()?,
            admin_api_key: random_api_key()?,
            admin: AdminConfig { enabled: true, ..Default::default() },
            credential_source: "env".into(),
            ..Default::default()
        })
    }

    pub fn from_env_and_optional_file(path: Option<&Path>) -> Result<Self, AppError> {
        let mut config = match path {
            Some(path) => {
                let text = fs::read_to_string(path).map_err(|error| {
                    AppError::Config(format!("cannot read {}: {error}", path.display()))
                })?;
                serde_json::from_str(&text)
                    .map_err(|error| AppError::Config(format!("invalid v2 config JSON: {error}")))?
            }
            None => Self::default(),
        };
        config.apply_env()?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self) -> Result<(), AppError> {
        reject_legacy_env()?;
        if let Some(value) = env_string("KIRO__ACCESS__CLIENT_API_KEY") {
            self.client_api_key = value;
        }
        if let Some(value) = env_string("KIRO__ADMIN__API_KEY") {
            self.admin_api_key = value;
        }
        if let Some(value) = env_bool("KIRO__ADMIN__ENABLED")? {
            self.admin.enabled = value;
        }
        if let Some(value) = env_parse("KIRO__ADMIN__SESSION_TTL_SECS")? {
            self.admin.session_ttl_secs = value;
        }
        if let Some(value) = env_bool("KIRO__ADMIN__COOKIE_SECURE")? {
            self.admin.cookie_secure = value;
        }
        if let Some(value) = env_string("KIRO__ADMIN__ALLOWED_ORIGINS") {
            self.admin.allowed_origins = split_list(&value);
        }
        if let Some(value) = env_parse("KIRO__ADMIN__LOGIN_RATE_LIMIT_PER_MINUTE")? {
            self.admin.login_rate_limit_per_minute = value;
        }
        if let Some(value) = env_bool("KIRO__ADMIN__TRUST_FORWARDED_HEADERS")? {
            self.trust_forwarded_headers = value;
        }
        if let Some(value) = env_string("KIRO__SERVER__HOST") {
            self.host = value;
        }
        if let Some(value) = env_parse("KIRO__SERVER__PORT")? {
            self.port = value;
        }
        if let Some(value) = env_parse("KIRO__SERVER__MAX_REQUEST_BODY_BYTES")? {
            self.max_request_body_bytes = value;
        }
        if let Some(value) = env_parse("KIRO__SERVER__GRACEFUL_SHUTDOWN_TIMEOUT_SECS")? {
            self.graceful_shutdown_timeout_secs = value;
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__SOURCE") {
            self.credential_source = value;
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__PATH") {
            self.credential_path = Some(value);
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__JSON_PATH") {
            self.credential_json_path = Some(value);
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__ACCESS_TOKEN") {
            self.credential_access_token = Some(value);
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__REFRESH_TOKEN") {
            self.credential_refresh_token = Some(value);
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__API_KEY") {
            self.credential_api_key = Some(value);
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__CLIENT_ID") {
            self.credential_client_id = Some(value);
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__CLIENT_SECRET") {
            self.credential_client_secret = Some(value);
        }
        if let Some(value) = env_string("KIRO__CREDENTIAL__MACHINE_ID") {
            self.credential_machine_id = Some(value);
        }
        if let Some(value) = env_string("KIRO__UPSTREAM__ENDPOINT") {
            self.endpoint = value;
        }
        if let Some(value) = env_string("KIRO__UPSTREAM__API_REGION") {
            self.api_region = value;
        }
        if let Some(value) = env_string("KIRO__UPSTREAM__URL") {
            self.upstream_url = Some(value);
        }
        if let Some(value) = env_string("KIRO__UPSTREAM__PROXY_URL") {
            self.proxy_url = Some(value);
        }
        if let Some(value) = env_string("KIRO__UPSTREAM__TOKEN_ENDPOINT") {
            self.token_endpoint = Some(value);
        }
        if let Some(value) = env_parse("KIRO__UPSTREAM__TIMEOUT_SECS")? {
            self.upstream_timeout_secs = value;
        }
        if let Some(value) = env_parse("KIRO__UPSTREAM__MAX_BODY_BYTES")? {
            self.max_upstream_body_bytes = value;
        }
        if let Some(value) = env_parse("KIRO__UPSTREAM__REFRESH_EARLY_SECS")? {
            self.refresh_early_secs = value;
        }
        if let Some(value) = env_parse("KIRO__UPSTREAM__REFRESH_INTERVAL_SECS")? {
            self.refresh_interval_secs = value;
        }
        if let Some(value) = env_string("KIRO__UPSTREAM__MCP_REGION") {
            self.mcp_region = Some(value);
        }
        if let Some(value) = env_parse("KIRO__MODELS__CACHE_TTL_SECS")? {
            self.model_cache_ttl_secs = value;
        }
        if let Some(value) = env_string("KIRO__MODELS__ALIASES") {
            self.model_aliases = parse_model_aliases(&value)?;
        }
        if let Some(value) = env_string("KIRO__STORAGE__RESPONSE_STORE_PATH") {
            self.response_store_path = value;
        }
        if let Some(value) = env_string("KIRO__LOGGING__FORMAT") {
            self.log_json = match value.as_str() {
                "text" => false,
                "json" => true,
                other => {
                    return Err(AppError::Config(format!("unsupported logging format: {other}")));
                }
            };
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.version != CONFIG_VERSION {
            return Err(AppError::Config(format!(
                "configuration version must be {CONFIG_VERSION}"
            )));
        }
        if self.host.trim().is_empty() {
            return Err(AppError::Config("server.host must not be empty".into()));
        }
        if self.port == 0 {
            return Err(AppError::Config("server.port must be non-zero".into()));
        }
        if self.client_api_key.trim().is_empty() {
            return Err(AppError::Config("access.client_api_key is required".into()));
        }
        if self.admin.enabled && self.admin_api_key.trim().is_empty() {
            return Err(AppError::Config("admin.api_key is required when admin is enabled".into()));
        }
        if self.admin.session_ttl_secs == 0 || self.admin.login_rate_limit_per_minute == 0 {
            return Err(AppError::Config("admin session and rate limits must be positive".into()));
        }
        if self.upstream_timeout_secs == 0 || self.refresh_interval_secs == 0 {
            return Err(AppError::Config("upstream timeouts must be positive".into()));
        }
        if self.max_request_body_bytes == 0 || self.max_upstream_body_bytes == 0 {
            return Err(AppError::Config("body size limits must be positive".into()));
        }
        if self.graceful_shutdown_timeout_secs == 0 || self.model_cache_ttl_secs == 0 {
            return Err(AppError::Config(
                "shutdown timeout and model cache TTL must be positive".into(),
            ));
        }
        if self.refresh_early_secs < 0 {
            return Err(AppError::Config(
                "upstream.refresh_early_secs must not be negative".into(),
            ));
        }
        if self.response_store_path.trim().is_empty() {
            return Err(AppError::Config("storage.response_store_path must not be empty".into()));
        }
        if self.api_region.trim().is_empty() {
            return Err(AppError::Config("upstream.api_region must not be empty".into()));
        }
        let has_api_key =
            self.credential_api_key.as_deref().is_some_and(|value| !value.trim().is_empty());
        let has_tokens =
            self.credential_access_token.as_deref().is_some_and(|value| !value.trim().is_empty())
                || self
                    .credential_refresh_token
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
        if has_api_key && has_tokens {
            return Err(AppError::Config(
                "credential.api_key cannot be combined with access_token or refresh_token".into(),
            ));
        }
        for (alias, target) in &self.model_aliases {
            if alias.trim().is_empty() || target.trim().is_empty() {
                return Err(AppError::Config("models.aliases entries must be non-empty".into()));
            }
        }
        if !matches!(
            self.credential_source.as_str(),
            "auto" | "env" | "json" | "sqlite" | "api_key"
        ) {
            return Err(AppError::Config(format!(
                "unsupported credential.source: {}",
                self.credential_source
            )));
        }
        if self.credential_source.trim().is_empty() {
            return Err(AppError::Config("credential.source must not be empty".into()));
        }
        if !matches!(self.endpoint.as_str(), "auto" | "ide" | "cli") {
            return Err(AppError::Config(format!(
                "unsupported upstream.endpoint: {}",
                self.endpoint
            )));
        }
        for (name, value) in [
            ("upstream.url", self.upstream_url.as_deref()),
            ("upstream.proxy_url", self.proxy_url.as_deref()),
            ("upstream.token_endpoint", self.token_endpoint.as_deref()),
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

fn random_api_key() -> Result<String, AppError> {
    const KEY_BYTES: usize = 32;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; KEY_BYTES];
    OsRng.try_fill_bytes(&mut bytes).map_err(|error| {
        AppError::Config(format!("cannot generate a secure random API key: {error}"))
    })?;
    let mut key = String::with_capacity(KEY_BYTES * 2);
    for byte in bytes {
        key.push(HEX[usize::from(byte >> 4)] as char);
        key.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    Ok(key)
}

fn env_string(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_model_aliases(value: &str) -> Result<HashMap<String, String>, AppError> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (alias, target) = entry.split_once('=').ok_or_else(|| {
                AppError::Config("KIRO__MODELS__ALIASES entries must use alias=target".into())
            })?;
            let alias = alias.trim();
            let target = target.trim();
            if alias.is_empty() || target.is_empty() {
                return Err(AppError::Config("model alias and target must be non-empty".into()));
            }
            Ok((alias.to_owned(), target.to_owned()))
        })
        .collect()
}

fn reject_legacy_env() -> Result<(), AppError> {
    const LEGACY: &[&str] = &[
        "KIRO_CLIENT_API_KEY",
        "KIRO_ADMIN_API_KEY",
        "KIRO_HOST",
        "KIRO_PORT",
        "KIRO_ENDPOINT",
        "KIRO_API_REGION",
        "KIRO_CREDENTIAL_SOURCE",
        "KIRO_CREDENTIAL_PATH",
        "KIRO_CREDENTIAL_JSON_PATH",
        "KIRO_UPSTREAM_URL",
        "KIRO_PROXY_URL",
        "KIRO_TOKEN_ENDPOINT",
        "KIRO_UPSTREAM_TIMEOUT_SECS",
        "KIRO_REFRESH_EARLY_SECS",
        "KIRO_REFRESH_INTERVAL_SECS",
        "KIRO_MODEL_CACHE_TTL_SECS",
        "KIRO_MODEL_ALIASES",
        "KIRO_MAX_REQUEST_BODY_BYTES",
        "KIRO_MAX_UPSTREAM_BODY_BYTES",
        "KIRO_GRACEFUL_SHUTDOWN_TIMEOUT_SECS",
        "KIRO_ADMIN_ENABLED",
        "KIRO_ADMIN_SESSION_TTL_SECS",
        "KIRO_ADMIN_COOKIE_SECURE",
        "KIRO_ADMIN_LOGIN_RATE_LIMIT_PER_MINUTE",
        "KIRO_MCP_REGION",
        "KIRO_LOG_JSON",
        "KIRO_TRUST_FORWARDED_HEADERS",
        "KIRO_RESPONSE_STORE_PATH",
        "KIRO_ACCESS_TOKEN",
        "KIRO_REFRESH_TOKEN",
        "KIRO_API_KEY",
        "KIRO_CLIENT_ID",
        "KIRO_CLIENT_SECRET",
        "KIRO_MACHINE_ID",
    ];
    if let Some(name) = LEGACY.iter().find(|name| env::var(name).is_ok()) {
        return Err(AppError::Config(format!(
            "legacy environment variable {name} is not supported; use KIRO__GROUP__FIELD"
        )));
    }
    const V2: &[&str] = &[
        "KIRO__ACCESS__CLIENT_API_KEY",
        "KIRO__ADMIN__API_KEY",
        "KIRO__ADMIN__ENABLED",
        "KIRO__ADMIN__SESSION_TTL_SECS",
        "KIRO__ADMIN__COOKIE_SECURE",
        "KIRO__ADMIN__ALLOWED_ORIGINS",
        "KIRO__ADMIN__LOGIN_RATE_LIMIT_PER_MINUTE",
        "KIRO__ADMIN__TRUST_FORWARDED_HEADERS",
        "KIRO__SERVER__HOST",
        "KIRO__SERVER__PORT",
        "KIRO__SERVER__MAX_REQUEST_BODY_BYTES",
        "KIRO__SERVER__GRACEFUL_SHUTDOWN_TIMEOUT_SECS",
        "KIRO__CREDENTIAL__SOURCE",
        "KIRO__CREDENTIAL__PATH",
        "KIRO__CREDENTIAL__JSON_PATH",
        "KIRO__CREDENTIAL__ACCESS_TOKEN",
        "KIRO__CREDENTIAL__REFRESH_TOKEN",
        "KIRO__CREDENTIAL__API_KEY",
        "KIRO__CREDENTIAL__CLIENT_ID",
        "KIRO__CREDENTIAL__CLIENT_SECRET",
        "KIRO__CREDENTIAL__MACHINE_ID",
        "KIRO__UPSTREAM__ENDPOINT",
        "KIRO__UPSTREAM__API_REGION",
        "KIRO__UPSTREAM__URL",
        "KIRO__UPSTREAM__PROXY_URL",
        "KIRO__UPSTREAM__TOKEN_ENDPOINT",
        "KIRO__UPSTREAM__TIMEOUT_SECS",
        "KIRO__UPSTREAM__MAX_BODY_BYTES",
        "KIRO__UPSTREAM__REFRESH_EARLY_SECS",
        "KIRO__UPSTREAM__REFRESH_INTERVAL_SECS",
        "KIRO__UPSTREAM__MCP_REGION",
        "KIRO__MODELS__CACHE_TTL_SECS",
        "KIRO__MODELS__ALIASES",
        "KIRO__STORAGE__RESPONSE_STORE_PATH",
        "KIRO__LOGGING__FORMAT",
    ];
    if let Some(name) = env::vars()
        .map(|(name, _)| name)
        .find(|name| name.starts_with("KIRO__") && !V2.iter().any(|allowed| *allowed == name))
    {
        return Err(AppError::Config(format!(
            "unknown v2 environment variable {name}; use a documented KIRO__GROUP__FIELD"
        )));
    }
    Ok(())
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
    use super::{AdminConfig, AppConfig, CONFIG_VERSION, parse_model_aliases};

    #[test]
    fn runtime_defaults_disable_admin() {
        assert!(!AppConfig::default().admin.enabled);
        assert_eq!(AppConfig::default().version, CONFIG_VERSION);
    }

    #[test]
    fn generated_templates_have_independent_random_keys_and_enable_admin() {
        let first = AppConfig::generated_template().unwrap();
        let second = AppConfig::generated_template().unwrap();
        assert!(first.admin.enabled);
        assert_eq!(first.credential_source, "env");
        assert_ne!(first.client_api_key, first.admin_api_key);
        assert_ne!(first.client_api_key, second.client_api_key);
        assert_ne!(first.admin_api_key, second.admin_api_key);
        let serialized = serde_json::to_value(&first).unwrap();
        assert_eq!(serialized["version"], CONFIG_VERSION);
        assert_eq!(serialized["credential"]["source"], "env");
        assert_eq!(serialized["logging"]["format"], "text");
        for key in [
            &first.client_api_key,
            &first.admin_api_key,
            &second.client_api_key,
            &second.admin_api_key,
        ] {
            assert_eq!(key.len(), 64);
            assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        assert!(first.validate().is_ok());
    }

    #[test]
    fn canonical_v2_fields_are_accepted() {
        let config: AppConfig = serde_json::from_str(
            r#"{"version":2,"access":{"client_api_key":"client"},"admin":{"api_key":"admin"},"upstream":{"endpoint":"cli"},"models":{"aliases":{"claude-sonnet-5":"@first"}}}"#,
        )
        .unwrap();
        assert_eq!(config.client_api_key, "client");
        assert_eq!(config.endpoint, "cli");
        assert_eq!(config.model_aliases["claude-sonnet-5"], "@first");
        assert!(config.admin.cookie_secure);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn explicit_zero_values_are_not_silently_replaced_with_defaults() {
        for value in [
            r#"{"version":2,"access":{"client_api_key":"client"},"server":{"port":0}}"#,
            r#"{"version":2,"access":{"client_api_key":"client"},"server":{"max_request_body_bytes":0}}"#,
            r#"{"version":2,"access":{"client_api_key":"client"},"upstream":{"timeout_secs":0}}"#,
            r#"{"version":2,"access":{"client_api_key":"client"},"models":{"cache_ttl_secs":0}}"#,
        ] {
            let config: AppConfig = serde_json::from_str(value).unwrap();
            assert!(config.validate().is_err(), "explicit zero unexpectedly accepted: {value}");
        }
    }

    #[test]
    fn old_flattened_json_and_wrong_versions_are_rejected() {
        for value in [
            r#"{"client_api_key":"client","admin_api_key":"admin"}"#,
            r#"{"version":1,"access":{"client_api_key":"client"}}"#,
            r#"{"version":2,"access":{"client_api_key":"client"},"unknown":{}}"#,
            r#"{"version":2,"access":{"client_api_key":"client"},"credential":{"api_region":"us-east-1"}}"#,
        ] {
            assert!(serde_json::from_str::<AppConfig>(value).is_err());
        }
    }

    #[test]
    fn model_aliases_require_alias_and_target() {
        assert!(parse_model_aliases("claude-sonnet-5=@first,haiku=gpt-5.6-sol").is_ok());
        assert!(parse_model_aliases("claude-sonnet-5").is_err());
        assert!(parse_model_aliases("=gpt-5.6-sol").is_err());
        assert!(parse_model_aliases("claude-sonnet-5=").is_err());
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
    fn enabled_admin_requires_an_admin_key() {
        let config = AppConfig {
            client_api_key: "client".into(),
            admin: AdminConfig { enabled: true, ..Default::default() },
            ..Default::default()
        };
        assert!(config.validate().is_err());
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
}
