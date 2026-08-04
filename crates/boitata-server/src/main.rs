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
    /// Directory of blueprint YAML files to offer by name in the API and web UI
    /// (e.g. examples/blueprints). Only these vetted files are runnable over the
    /// network; omit to run the single-agent path only.
    #[arg(long)]
    blueprints_dir: Option<String>,
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

    // Discover the blueprints the server will offer by name (none unless
    // `--blueprints-dir` is set). Each file is compiled here, so a malformed
    // blueprint is a startup error rather than a surprise mid-run.
    let blueprints = match &args.blueprints_dir {
        Some(dir) => {
            let found = boitata_orchestrator::discover(std::path::Path::new(dir))
                .with_context(|| format!("failed to load blueprints from {dir}"))?;
            info!(
                "Loaded {} blueprint(s) from {dir}: [{}]",
                found.len(),
                found.keys().cloned().collect::<Vec<_>>().join(", ")
            );
            found
        }
        None => Default::default(),
    };

    let state = AppState::new(config, provider, tools, policy, blueprints);
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .with_context(|| format!("failed to bind {}", args.addr))?;
    // The server has no auth and drives an agent with shell/file/git tools, so
    // binding to a non-loopback address exposes it to the whole network.
    if !listener
        .local_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false)
    {
        tracing::warn!(
            "listening on non-loopback address {} — there is no authentication; \
             put it behind a trusted network or an authenticating reverse proxy",
            args.addr
        );
    }
    info!("boitata-server listening on http://{}", args.addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve on Ctrl-C (SIGINT) or, on Unix, SIGTERM — the signal container
/// runtimes send — so `axum::serve` stops accepting connections and drains
/// in-flight requests before the process exits.
async fn shutdown_signal() {
    let ctrl_c = async {
        // If the handler can't be installed, never resolve — otherwise this
        // branch would fire immediately and shut the server down at startup.
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // If we can't install the handler, never resolve this branch.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    info!("shutdown signal received; draining connections");
}
