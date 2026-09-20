use crate::{
    auth::{AuthMethod, Credential},
    error::AppError,
};
use parking_lot::RwLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_limits: Option<TokenLimits>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAvailableModelsResponse {
    #[serde(default)]
    models: Vec<ModelInfo>,
}

struct CacheEntry {
    fetched_at: Instant,
    result: Result<Vec<ModelInfo>, String>,
}

#[derive(Clone)]
pub struct ModelCatalog {
    client: Client,
    ttl: Duration,
    base_url: Option<String>,
    cache: Arc<RwLock<Option<CacheEntry>>>,
    refresh_lock: Arc<Mutex<()>>,
}

impl ModelCatalog {
    pub fn new(client: Client, ttl: Duration) -> Self {
        Self {
            client,
            ttl,
            base_url: None,
            cache: Arc::new(RwLock::new(None)),
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn get(&self, credential: &Credential) -> Result<Vec<ModelInfo>, AppError> {
        if let Some(result) = self.cached_result() {
            return result;
        }

        let _guard = self.refresh_lock.lock().await;
        if let Some(result) = self.cached_result() {
            return result;
        }

        let result = self.fetch(credential).await;
        let cached = match &result {
            Ok(models) => Ok(models.clone()),
            Err(error) => Err(error.to_string()),
        };
        *self.cache.write() = Some(CacheEntry { fetched_at: Instant::now(), result: cached });
        result
    }

    async fn fetch(&self, credential: &Credential) -> Result<Vec<ModelInfo>, AppError> {
        let regions = rest_api_region_candidates(credential.sso_region.as_deref());
        let token = credential
            .access_token
            .as_ref()
            .filter(|token| !token.is_empty())
            .map(|token| token.expose_secret())
            .ok_or_else(|| {
                AppError::Credential("no usable access token for model discovery".into())
            })?;

        let mut last_status = None;
        for (index, region) in regions.iter().enumerate() {
            let base_url = self
                .base_url
                .as_deref()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("https://q.{region}.amazonaws.com"));
            let mut url =
                url::Url::parse(&format!("{base_url}/ListAvailableModels")).map_err(|error| {
                    AppError::Upstream(format!("invalid model discovery URL: {error}"))
                })?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("origin", "AI_EDITOR");
                query.append_pair("maxResults", "50");
            }
            let host = url.host_str().unwrap_or_default().to_owned();
            let machine_suffix = if credential.machine_id.is_empty() {
                String::new()
            } else {
                format!("-{}", credential.machine_id)
            };
            let mut request = self
                .client
                .get(url)
                .bearer_auth(token)
                .header("accept", "application/json")
                .header(
                    "x-amz-user-agent",
                    format!("aws-sdk-js/1.0.0 KiroIDE-0.9.2{machine_suffix}"),
                )
                .header(
                    "user-agent",
                    format!(
                        "aws-sdk-js/1.0.0 ua/2.1 os/linux lang/js md/nodejs#20 \
                         api/codewhispererruntime#1.0.0 m/N,E KiroIDE-0.9.2{machine_suffix}"
                    ),
                )
                .header("host", &host)
                .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
                .header("amz-sdk-request", "attempt=1; max=1")
                .header("x-amzn-codewhisperer-optout", "true")
                .header("connection", "close");
            if matches!(credential.auth_method, AuthMethod::ApiKey) {
                request = request.header("tokentype", "API_KEY");
            } else if matches!(credential.auth_method, AuthMethod::Social) {
                request = request.header("TokenType", "EXTERNAL_IDP");
            }

            let response = request.send().await.map_err(|error| {
                AppError::Upstream(format!("model discovery request failed: {error}"))
            })?;
            let status = response.status();
            if status.is_success() {
                let body = response.bytes().await.map_err(|error| {
                    AppError::Upstream(format!("model discovery response failed: {error}"))
                })?;
                let parsed = serde_json::from_slice::<ListAvailableModelsResponse>(&body).map_err(
                    |error| {
                        AppError::Upstream(format!("invalid model discovery response: {error}"))
                    },
                )?;
                return Ok(parsed.models);
            }
            if status == reqwest::StatusCode::FORBIDDEN && index + 1 < regions.len() {
                last_status = Some(status);
                continue;
            }
            return Err(AppError::Upstream(format!("model discovery returned {status}")));
        }

        Err(AppError::Upstream(format!(
            "model discovery returned {}",
            last_status.unwrap_or(reqwest::StatusCode::FORBIDDEN)
        )))
    }

    fn cached_result(&self) -> Option<Result<Vec<ModelInfo>, AppError>> {
        let cache = self.cache.read();
        let entry = cache.as_ref()?;
        if entry.fetched_at.elapsed() >= self.ttl {
            return None;
        }
        Some(entry.result.clone().map_err(AppError::Upstream))
    }

    pub(crate) fn invalidate(&self) {
        *self.cache.write() = None;
    }

    #[cfg(test)]
    pub(crate) fn has_cached_result(&self) -> bool {
        self.cached_result().is_some()
    }

    #[cfg(test)]
    pub fn seed(&self, models: Vec<ModelInfo>) {
        *self.cache.write() = Some(CacheEntry { fetched_at: Instant::now(), result: Ok(models) });
    }

    #[cfg(test)]
    fn with_base_url(client: Client, ttl: Duration, base_url: String) -> Self {
        Self {
            client,
            ttl,
            base_url: Some(base_url),
            cache: Arc::new(RwLock::new(None)),
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }
}

fn rest_api_region_candidates(sso_region: Option<&str>) -> [&'static str; 2] {
    if sso_region.is_some_and(|region| region.starts_with("eu-")) {
        ["eu-central-1", "us-east-1"]
    } else {
        ["us-east-1", "eu-central-1"]
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelCatalog, ModelInfo, TokenLimits, rest_api_region_candidates};
    use crate::auth::{AuthMethod, Credential, SecretString};
    use std::{env, fs, path::PathBuf, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn selects_eu_rest_endpoint_first_for_eu_sso_regions() {
        assert_eq!(rest_api_region_candidates(Some("eu-west-1")), ["eu-central-1", "us-east-1"]);
        assert_eq!(rest_api_region_candidates(Some("us-east-2")), ["us-east-1", "eu-central-1"]);
        assert_eq!(rest_api_region_candidates(None), ["us-east-1", "eu-central-1"]);
    }

    #[test]
    fn deserializes_available_model_metadata() {
        let response: super::ListAvailableModelsResponse = serde_json::from_str(
            r#"{"models":[{"modelId":"claude-sonnet-4.5","modelName":"Sonnet","description":"balanced","tokenLimits":{"maxInputTokens":200000,"maxOutputTokens":8192}}]}"#,
        )
        .unwrap();
        assert_eq!(
            response.models,
            vec![ModelInfo {
                model_id: "claude-sonnet-4.5".into(),
                model_name: Some("Sonnet".into()),
                description: Some("balanced".into()),
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(200000),
                    max_output_tokens: Some(8192),
                }),
            }]
        );
    }

    #[tokio::test]
    async fn fetches_and_caches_models_from_remote_api() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let length = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..length]).to_ascii_lowercase();
            assert!(request.starts_with("get /listavailablemodels?"));
            assert!(request.contains("origin=ai_editor"));
            assert!(request.contains("maxresults=50"));
            assert!(request.contains("authorization: bearer token"));
            assert!(request.contains("kiroide-0.9.2"));
            let body = br#"{"models":[{"modelId":"claude-sonnet-4.5","description":"balanced"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        let catalog = ModelCatalog::with_base_url(
            reqwest::Client::new(),
            Duration::from_secs(300),
            format!("http://{address}"),
        );
        let credential = Credential {
            auth_method: AuthMethod::Oidc,
            access_token: Some(SecretString::new("token")),
            profile_arn: Some("arn:aws:codewhisperer:us-east-1:123:profile/test".into()),
            ..Default::default()
        };

        let first = catalog.get(&credential).await.unwrap();
        let second = catalog.get(&credential).await.unwrap();
        server.await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first[0].model_id, "claude-sonnet-4.5");
    }

    #[tokio::test]
    async fn retries_another_region_after_forbidden() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (status, body) in
                [(403, Vec::new()), (200, br#"{"models":[{"modelId":"fallback"}]}"#.to_vec())]
            {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status} {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    if status == 200 { "OK" } else { "Forbidden" },
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let catalog = ModelCatalog::with_base_url(
            reqwest::Client::new(),
            Duration::from_secs(300),
            format!("http://{address}"),
        );
        let credential = Credential {
            auth_method: AuthMethod::Oidc,
            access_token: Some(SecretString::new("token")),
            ..Default::default()
        };

        let models = catalog.get(&credential).await.unwrap();
        server.await.unwrap();
        assert_eq!(
            models.iter().map(|model| model.model_id.as_str()).collect::<Vec<_>>(),
            ["fallback"]
        );
    }

    #[tokio::test]
    async fn refreshes_after_ttl_expires() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for model_id in ["first", "second"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await.unwrap();
                let body = format!(r#"{{"models":[{{"modelId":"{model_id}"}}]}}"#);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(body.as_bytes()).await.unwrap();
            }
        });
        let catalog = ModelCatalog::with_base_url(
            reqwest::Client::new(),
            Duration::from_millis(10),
            format!("http://{address}"),
        );
        let credential = Credential {
            auth_method: AuthMethod::Oidc,
            access_token: Some(SecretString::new("token")),
            ..Default::default()
        };

        let first = catalog.get(&credential).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let second = catalog.get(&credential).await.unwrap();
        server.await.unwrap();
        assert_eq!(first[0].model_id, "first");
        assert_eq!(second[0].model_id, "second");
    }

    #[tokio::test]
    async fn coalesces_concurrent_initial_requests() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let body = br#"{"models":[{"modelId":"shared"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        let catalog = ModelCatalog::with_base_url(
            reqwest::Client::new(),
            Duration::from_secs(300),
            format!("http://{address}"),
        );
        let credential = Credential {
            auth_method: AuthMethod::Oidc,
            access_token: Some(SecretString::new("token")),
            ..Default::default()
        };
        let requests = (0..8)
            .map(|_| {
                let catalog = catalog.clone();
                let credential = credential.clone();
                tokio::spawn(async move { catalog.get(&credential).await })
            })
            .collect::<Vec<_>>();

        for request in requests {
            assert_eq!(request.await.unwrap().unwrap()[0].model_id, "shared");
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn caches_an_empty_successful_model_list() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let body = br#"{"models":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        let catalog = ModelCatalog::with_base_url(
            reqwest::Client::new(),
            Duration::from_secs(300),
            format!("http://{address}"),
        );
        let credential = Credential {
            auth_method: AuthMethod::Oidc,
            access_token: Some(SecretString::new("token")),
            ..Default::default()
        };

        assert!(catalog.get(&credential).await.unwrap().is_empty());
        assert!(catalog.get(&credential).await.unwrap().is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a local Kiro CLI SQLite database and live Kiro API access"]
    async fn discovers_models_from_real_kiro_sqlite_credentials() {
        let path = env::var_os("KIRO_REAL_SQLITE_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local/share/kiro-cli/data.sqlite3"))
            })
            .expect("HOME or KIRO_REAL_SQLITE_PATH must be set");
        assert!(path.is_file(), "SQLite database does not exist");
        let before = fs::metadata(&path).unwrap();
        let credential = crate::auth::sqlite::load(&path)
            .expect("real Kiro CLI SQLite database should be readable")
            .into_iter()
            .next()
            .expect("real Kiro CLI SQLite database should contain a credential");
        let config = crate::config::AppConfig::default();
        let token_manager = crate::credential::TokenManager::new(&config, credential)
            .expect("real credential should initialize model discovery");
        let models = token_manager
            .available_models()
            .await
            .expect("real Kiro model discovery should succeed");
        let after = fs::metadata(&path).unwrap();

        assert!(!models.is_empty());
        assert!(models.iter().all(|model| !model.model_id.is_empty()));
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().ok(), after.modified().ok());
    }
}
