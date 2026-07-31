//! `boitata-server`: an HTTP/SSE backend and embedded web UI for running agent
//! tasks and blueprints from a browser.
//!
//! It reuses the same runtime assembly as the CLI (`boitata_core::runtime`):
//! build the provider, register built-in + MCP tools, and build the permission
//! policy once at startup, then serve them to any number of concurrent runs.

mod api;
mod assets;
mod events;
mod state;

use anyhow::Context;
use boitata_core::config::Config;
use boitata_core::runtime;
use clap::Parser;
use tracing::info;

use crate::state::AppState;

#[derive(Parser)]
#[command(name = "boitata-server")]
#[command(about = "HTTP/SSE backend and web UI for boitata", long_about = None)]
struct Args {
    /// Path to the config file (default: boitata.toml, or $BOITATA_CONFIG)
    #[arg(long)]
    config: Option<String>,
    /// Address to bind, e.g. 127.0.0.1:8787 or 0.0.0.0:8787
    #[arg(long, default_value = "127.0.0.1:8787")]
    addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let args = Args::parse();

    let path = Config::resolve_path(args.config);
    let config = Config::load(&path)?;
    info!(
        "Loaded config from {} (provider={}, model={})",
        path.display(),
        config.provider,
        config.model
    );

    // Same wiring the CLI uses; built once and shared across runs.
    runtime::init_workspace(&config);
    let provider = runtime::build_provider(&config)?;
    let tools = runtime::build_tools(&config).await?;
    let policy = runtime::build_policy(&config)?;

    let state = AppState::new(config, provider, tools, policy);
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .with_context(|| format!("failed to bind {}", args.addr))?;
    info!("boitata-server listening on http://{}", args.addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve on Ctrl-C (SIGINT) so `axum::serve` stops accepting connections and
/// drains in-flight requests before the process exits.
async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        info!("shutdown signal received; draining connections");
    }
}
