use crate::{
    AppState, admin,
    common::request_id,
    http::{anthropic, openai, responses},
};
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use tower_http::limit::RequestBodyLimitLayer;

pub fn router(state: AppState) -> Router {
    let health_route = Router::new().route("/health", get(health));
    let protected = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/messages", post(anthropic::route))
        .route("/v1/messages/count_tokens", post(anthropic::count_tokens_route))
        .route("/v1/chat/completions", post(openai::route))
        .route("/v1/responses", post(responses::create_route))
        .route("/v1/responses/{id}", get(responses::get).delete(responses::delete))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::admin::middleware::client_api_key,
        ));
    let mut router = Router::new()
        .merge(health_route)
        .merge(protected)
        .layer(axum::middleware::from_fn(request_id::request_id));
    if state.config.admin.enabled {
        let admin_routes = admin::router::router(state.clone());
        router = router
            .route("/admin", get(crate::admin_ui::index))
            .route("/admin/app.js", get(crate::admin_ui::app_js))
            .route("/admin/styles.css", get(crate::admin_ui::styles_css))
            .nest("/admin", admin_routes.clone())
            .nest("/api/admin", admin_routes);
    }
    router.layer(RequestBodyLimitLayer::new(state.config.max_request_body_bytes)).with_state(state)
}

pub async fn health(State(_state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        axum::Json(
            serde_json::json!({"ok":true,"service":"kiro-gateway","build_epoch":env!("KIRO_BUILD_EPOCH")}),
        ),
    )
}
async fn models(State(state): State<AppState>) -> impl IntoResponse {
    let models = match state.token_manager.available_models().await {
        Ok(models) => models,
        Err(_error) => {
            tracing::warn!(error_class = "model_discovery", "model discovery failed");
            Vec::new()
        }
    };
    let data = models
        .into_iter()
        .map(|model| {
            let mut value =
                serde_json::json!({"id":model.model_id,"object":"model","owned_by":"kiro"});
            if let Some(name) = model.model_name {
                value["model_name"] = serde_json::Value::String(name);
            }
            if let Some(description) = model.description {
                value["description"] = serde_json::Value::String(description);
            }
            if let Some(token_limits) = model.token_limits {
                value["token_limits"] = serde_json::to_value(token_limits).unwrap_or_default();
            }
            value
        })
        .collect::<Vec<_>>();
    axum::Json(serde_json::json!({"object":"list","data":data}))
}

#[cfg(test)]
mod tests {
    use super::router;
    use crate::{
        AppState,
        app_state::build_upstream,
        auth::{AuthMethod, Credential},
        config::{AdminConfig, AppConfig},
        credential::TokenManager,
        model_catalog::{ModelInfo, TokenLimits},
        response_store::ResponseStore,
    };
    use axum::body::Body;
    use http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn disabled_admin_has_no_page_or_api_routes() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin: AdminConfig { enabled: false, ..Default::default() },
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager: Arc::new(TokenManager::new(&config, credential).unwrap()),
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };
        let app = router(state);
        let page = app
            .clone()
            .oneshot(Request::builder().uri("/admin").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::NOT_FOUND);
        let api = app
            .clone()
            .oneshot(Request::builder().uri("/api/admin/credential").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::NOT_FOUND);
        let health = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn enabled_admin_page_is_public_before_login() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            admin: AdminConfig { enabled: true, ..Default::default() },
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager: Arc::new(TokenManager::new(&config, credential).unwrap()),
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };
        let app = router(state);
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/admin").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_SECURITY_POLICY)
                .and_then(|value| value.to_str().ok()),
            Some(
                "default-src 'self'; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"
            ),
        );
        let asset = app
            .oneshot(Request::builder().uri("/admin/app.js").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(
            asset.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn models_route_returns_remote_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let token_manager = Arc::new(TokenManager::new(&config, credential).unwrap());
        token_manager.seed_models_for_tests(vec![ModelInfo {
            model_id: "claude-sonnet-4.5".into(),
            model_name: Some("Sonnet".into()),
            description: Some("balanced".into()),
            token_limits: Some(TokenLimits {
                max_input_tokens: Some(200_000),
                max_output_tokens: Some(8_192),
            }),
        }]);
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager,
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("x-api-key", "client")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "list");
        assert_eq!(json["data"][0]["id"], "claude-sonnet-4.5");
        assert_eq!(json["data"][0]["model_name"], "Sonnet");
        assert_eq!(json["data"][0]["token_limits"]["maxInputTokens"], 200_000);
    }

    #[tokio::test]
    async fn models_route_returns_empty_data_when_discovery_fails() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager: Arc::new(TokenManager::new(&config, credential).unwrap()),
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("x-api-key", "client")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"object":"list","data":[]}));
    }

    #[tokio::test]
    async fn request_body_limit_is_enforced_before_json_parsing() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            max_request_body_bytes: 16,
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager: Arc::new(TokenManager::new(&config, credential).unwrap()),
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", "client")
                    .header("content-type", "application/json")
                    .body(Body::from("x".repeat(128)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn protocol_json_errors_use_chat_error_contract() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager: Arc::new(TokenManager::new(&config, credential).unwrap()),
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("x-api-key", "client")
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"].get("message").is_some());
    }

    #[tokio::test]
    async fn invalid_client_key_uses_anthropic_error_contract() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager: Arc::new(TokenManager::new(&config, credential).unwrap()),
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/messages")
                    .header("x-api-key", "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn admin_login_rate_limit_returns_429() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            client_api_key: "client".into(),
            admin_api_key: "admin".into(),
            admin: AdminConfig {
                enabled: true,
                login_rate_limit_per_minute: 1,
                cookie_secure: false,
                ..Default::default()
            },
            response_store_path: directory.path().join("responses.sqlite3").display().to_string(),
            ..Default::default()
        };
        let credential = Credential { auth_method: AuthMethod::ApiKey, ..Default::default() };
        let state = AppState {
            config: Arc::new(config.clone()),
            token_manager: Arc::new(TokenManager::new(&config, credential).unwrap()),
            responses: ResponseStore::open(&config.response_store_path).unwrap(),
            upstream: build_upstream(&config, reqwest::Client::new()),
            sessions: Default::default(),
        };
        let app = router(state);
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/api/admin/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"api_key":"admin"}"#))
                .unwrap()
        };
        let first = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::NO_CONTENT);
        let second = app.oneshot(request()).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
