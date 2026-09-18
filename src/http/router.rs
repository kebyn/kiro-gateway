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

pub fn router(state: AppState) -> Router {
    let health_route = Router::new().route("/health", get(health));
    let protected = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/messages", post(anthropic::messages))
        .route("/v1/messages/count_tokens", post(anthropic::count_tokens))
        .route("/v1/chat/completions", post(openai::chat_completions))
        .route("/v1/responses", post(responses::create))
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
            .nest("/admin", admin_routes.clone())
            .nest("/api/admin", admin_routes);
    }
    router.with_state(state)
}

pub async fn health(State(_state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        axum::Json(
            serde_json::json!({"ok":true,"service":"kiro-gateway-rs","build_epoch":env!("KIRO_BUILD_EPOCH")}),
        ),
    )
}
async fn models() -> impl IntoResponse {
    axum::Json(
        serde_json::json!({"object":"list","data":[{"id":"kiro","object":"model","owned_by":"kiro"}]}),
    )
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
            .oneshot(Request::builder().uri("/admin").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
