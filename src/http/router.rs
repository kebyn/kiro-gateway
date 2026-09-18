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
        .layer(axum::middleware::from_fn_with_state(
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
