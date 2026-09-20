use crate::{
    auth,
    config::AppConfig,
    credential::TokenManager,
    endpoint::EndpointPolicy,
    error::AppError,
    protocol::internal::{InternalRequest, InternalResponse},
    response_store::ResponseStore,
    upstream::request::{InternalEventStream, UpstreamClient},
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
    pub async fn event_stream(
        &self,
        request: &InternalRequest,
    ) -> Result<InternalEventStream, AppError> {
        self.token_manager.ensure_fresh().await?;
        let credential = self.token_manager.credential();
        if credential.access_token.as_ref().is_none_or(|token| token.is_empty()) {
            return Err(AppError::Credential(
                "no usable upstream access token is configured".into(),
            ));
        }
        self.upstream.event_stream(request, &credential).await
    }

    pub async fn complete(&self, request: &InternalRequest) -> Result<InternalResponse, AppError> {
        let events = self.event_stream(request).await?;
        let response = crate::http::stream::drive(events, |_| Ok(())).await?;
        if response.text.is_empty()
            && response.thinking.is_empty()
            && response.tool_calls.is_empty()
            && response.stop_reason.is_none()
        {
            return Err(AppError::Integrity("upstream stream was empty".into()));
        }
        Ok(response)
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
    Arc::new(UpstreamClient::with_policy_and_limit(
        client,
        EndpointPolicy::parse(&config.endpoint),
        config.upstream_url.clone(),
        config.max_upstream_body_bytes,
    ))
}
