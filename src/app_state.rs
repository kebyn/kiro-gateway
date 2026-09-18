use crate::{
    auth,
    config::AppConfig,
    credential::TokenManager,
    endpoint::{EndpointKind, endpoint_for},
    error::AppError,
    protocol::internal::{InternalRequest, InternalResponse},
    response_store::ResponseStore,
    upstream::request::UpstreamClient,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub token_manager: Arc<TokenManager>,
    pub responses: ResponseStore,
    pub upstream: Arc<UpstreamClient>,
    pub sessions: crate::admin::session::SessionStore,
}

impl AppState {
    pub async fn complete(&self, request: &InternalRequest) -> Result<InternalResponse, AppError> {
        self.token_manager.ensure_fresh().await?;
        let credential = self.token_manager.credential();
        if credential.access_token.is_none() {
            return Ok(InternalResponse {
                text: format!(
                    "Kiro gateway is configured; upstream is not available for model {}. Request received: {}",
                    request.model,
                    request.last_user_text()
                ),
                stop_reason: Some("end_turn".into()),
                usage: Some(crate::protocol::internal::Usage::new(
                    0,
                    request.input_text().chars().count() as u64,
                )),
                ..Default::default()
            });
        }
        self.upstream.complete(request, &credential).await
    }

    pub async fn reload_credential(&self) -> Result<(), AppError> {
        let candidates = auth::source::discover(&self.config)?;
        if candidates.len() != 1 {
            return Err(AppError::Credential("reload requires exactly one credential".into()));
        }
        self.token_manager.replace_credential(candidates.into_iter().next().unwrap().credential);
        Ok(())
    }
}

pub fn build_upstream(config: &AppConfig, client: reqwest::Client) -> Arc<UpstreamClient> {
    Arc::new(UpstreamClient::new(
        client,
        endpoint_for(EndpointKind::parse(&config.endpoint), config.upstream_url.as_deref()),
    ))
}
