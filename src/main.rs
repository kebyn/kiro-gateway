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
mod model_catalog;
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
use tokio::sync::oneshot;

#[derive(Debug, Parser)]
#[command(name = "kiro-gateway", version, about = "Single-tenant Kiro API gateway")]
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
    let mut candidates = discover(&config)?;
    let credential = candidates.pop().ok_or("no credential")?.credential;
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
    let refresh_task =
        token_manager.spawn_refresh_task(Duration::from_secs(config.refresh_interval_secs));
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?;
    tracing::info!("kiro-gateway listening on {}:{}", config.host, config.port);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = axum::serve(listener, http::router::router(state))
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .into_future();
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => result,
        _ = shutdown_signal() => {
            let _ = shutdown_tx.send(());
            match tokio::time::timeout(
                Duration::from_secs(config.graceful_shutdown_timeout_secs),
                &mut server,
            ).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!("graceful shutdown timed out; active streams were cancelled");
                    Ok(())
                }
            }
        }
    };
    refresh_task.abort();
    let _ = refresh_task.await;
    result?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(%error, "failed to install SIGTERM handler; falling back to Ctrl-C");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn init_tracing(json: bool) {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    if json {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
