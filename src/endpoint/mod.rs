pub mod cli;
pub mod ide;

use crate::{auth::Credential, error::AppError, protocol::internal::InternalRequest};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    Ide,
    Cli,
}

impl EndpointKind {
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("cli") { Self::Cli } else { Self::Ide }
    }
}

pub trait KiroEndpoint: Send + Sync {
    fn kind(&self) -> EndpointKind;
    fn api_url(&self, credential: &Credential) -> String;
    fn mcp_url(&self, credential: &Credential) -> String;
    fn transform_api_body(
        &self,
        request: &InternalRequest,
        credential: &Credential,
    ) -> serde_json::Value;
    fn decorate_api(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder;
    fn decorate_mcp(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &Credential,
    ) -> reqwest::RequestBuilder;
    fn classify_error(&self, status: reqwest::StatusCode, body: &str) -> AppError;
}

pub fn endpoint_for(kind: EndpointKind, upstream_url: Option<&str>) -> Box<dyn KiroEndpoint> {
    match kind {
        EndpointKind::Cli => Box::new(cli::CliEndpoint::new(upstream_url.map(str::to_owned))),
        EndpointKind::Ide => Box::new(ide::IdeEndpoint::new(upstream_url.map(str::to_owned))),
    }
}
