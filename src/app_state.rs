use crate::{
    auth,
    config::AppConfig,
    credential::TokenManager,
    endpoint::{EndpointKind, endpoint_for},
    error::AppError,
    protocol::internal::{InternalEvent, InternalRequest, InternalResponse, Usage},
    response_store::ResponseStore,
    upstream::request::{InternalEventStream, UpstreamClient},
};
use futures_util::stream;
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
        if credential.access_token.is_none() {
            let text = format!(
                "Kiro gateway is configured; upstream is not available for model {}. Request received: {}",
                request.model,
                request.last_user_text()
            );
            let events = vec![
                Ok(InternalEvent::TextDelta { text }),
                Ok(InternalEvent::Usage {
                    usage: Usage::new(0, request.input_text().chars().count() as u64),
                }),
                Ok(InternalEvent::Stop { reason: "end_turn".into() }),
            ];
            return Ok(Box::pin(stream::iter(events)));
        }
        self.upstream.event_stream(request, &credential).await
    }

    pub async fn complete(&self, request: &InternalRequest) -> Result<InternalResponse, AppError> {
        let mut events = self.event_stream(request).await?;
        let mut accumulator = crate::upstream::request::InternalEventAccumulator::new();
        while let Some(event) = futures_util::StreamExt::next(&mut events).await {
            accumulator.push(event?)?;
        }
        Ok(accumulator.finish())
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
