use crate::{
    AppState,
    admin::{handlers, health, middleware as admin_middleware},
};
use axum::{
    Router, middleware,
    routing::{get, post},
};

pub fn router(state: AppState) -> Router<AppState> {
    if !state.config.admin.enabled {
        return Router::new().with_state(state);
    }
    let protected = Router::new()
        .route("/auth/logout", post(handlers::logout))
        .route("/auth/session", get(handlers::session))
        .route("/health", get(crate::http::router::health))
        .route("/provider_health", get(health::provider_health))
        .route("/credential", get(handlers::credential))
        .route("/credential/reload", post(handlers::credential_reload))
        .route("/credential/refresh", post(handlers::credential_refresh))
        .route("/responses/{id}/events", get(handlers::response_events))
        .route("/responses/{id}", get(handlers::response).delete(handlers::delete_response))
        .layer(middleware::from_fn_with_state(state.clone(), admin_middleware::admin_session));
    Router::new().route("/auth/login", post(handlers::login)).merge(protected).with_state(state)
}
