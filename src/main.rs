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
use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Debug, Parser)]
#[command(
    name = "kiro-gateway",
    version,
    about = "Single-tenant Kiro API gateway",
    after_help = "Runtime configuration:\n  Set KIRO_CLIENT_API_KEY for /v1 authentication.\n  Configure an upstream credential with KIRO_ACCESS_TOKEN, KIRO_REFRESH_TOKEN, or KIRO_API_KEY, or select a JSON/SQLite credential source.\n  Admin is disabled by default; KIRO_ADMIN_API_KEY is required only when Admin is enabled.\n  --generate-config creates random client and Admin keys, but does not create upstream Kiro credentials."
)]
struct Args {
    #[arg(long, env = "KIRO_CONFIG", help = "Load configuration from this JSON file")]
    config: Option<PathBuf>,
    #[arg(
        long,
        default_value_t = false,
        conflicts_with = "generate_config",
        help = "Validate and print the effective configuration, then exit"
    )]
    check_config: bool,
    #[arg(
        long,
        value_name = "PATH",
        num_args = 0..=1,
        conflicts_with = "check_config",
        help = "Generate a configuration with random gateway keys (prints to stdout when PATH is omitted)"
    )]
    generate_config: Option<Option<PathBuf>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if let Some(path) = args.generate_config.as_ref() {
        generate_config(path.as_deref())?;
        return Ok(());
    }
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

fn generate_config(path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    const CREDENTIAL_NOTICE: &str = "Set an upstream Kiro credential with KIRO_ACCESS_TOKEN, KIRO_REFRESH_TOKEN, or KIRO_API_KEY before starting the gateway.";

    let config = AppConfig::generated_template()?;
    let mut json = serde_json::to_string_pretty(&config)?;
    json.push('\n');

    if let Some(path) = path {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot create generated config {}: {error}", path.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot set permissions on {}: {error}", path.display()),
                )
            })?;
        }
        file.write_all(json.as_bytes()).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot write generated config {}: {error}", path.display()),
            )
        })?;
        eprintln!("Generated configuration at {}. {CREDENTIAL_NOTICE}", path.display());
    } else {
        io::stdout().lock().write_all(json.as_bytes())?;
        eprintln!("Generated configuration contains sensitive gateway keys. {CREDENTIAL_NOTICE}");
    }
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
