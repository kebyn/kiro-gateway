mod admin;
mod admin_ui;
mod app_state;
mod auth;
mod common;
mod config;
mod credential;
mod endpoint;
mod error;
mod http;
mod protocol;
mod response_store;
mod transform;
mod upstream;

use app_state::{AppState, build_upstream};
use auth::source::discover;
use clap::Parser;
use common::redaction::redact_json;
use config::AppConfig;
use credential::TokenManager;
use response_store::ResponseStore;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Debug, Parser)]
#[command(name = "kiro-gateway-rs", version, about = "Single-tenant Kiro API gateway")]
struct Args {
    #[arg(long, env = "KIRO_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let config = AppConfig::from_env_and_optional_file(args.config.as_deref())?;
    init_tracing(config.log_json);
    if args.check_config {
        let mut output = serde_json::to_value(&config)?;
        redact_json(&mut output);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    let candidates = discover(&config)?;
    let credential = candidates.into_iter().next().ok_or("no credential")?.credential;
    let token_manager = Arc::new(TokenManager::new(&config, credential)?);
    token_manager.ensure_fresh().await?;
    let response_store =
        ResponseStore::open(AppConfig::expanded_path(&config.response_store_path))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.upstream_timeout_secs))
        .build()?;
    let state = AppState {
        config: Arc::new(config.clone()),
        token_manager: token_manager.clone(),
        responses: response_store,
        upstream: build_upstream(&config, client),
        sessions: Default::default(),
    };
    let _refresh_task =
        token_manager.spawn_refresh_task(Duration::from_secs(config.refresh_interval_secs));
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?;
    tracing::info!("kiro-gateway-rs listening on {}:{}", config.host, config.port);
    axum::serve(listener, http::router::router(state)).await?;
    Ok(())
}

fn init_tracing(json: bool) {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    if json {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
