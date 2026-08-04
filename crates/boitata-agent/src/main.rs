//! `boitata-agent`: the agent exposed as an ACP server over TCP.
//!
//! This is the process that runs *inside* a sandbox/VM. It builds a provider +
//! tools from local config (the same `boitata_core::runtime` wiring the CLI and
//! server use), then serves the agent over the Agent Client Protocol so an
//! orchestrator can drive it and stream its events back.

use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use boitata_acp::{AgentOutcome, PromptRunner, serve};
use boitata_agent::{Agent, Task};
use boitata_core::audit::AuditSink;
use boitata_core::config::Config;
use boitata_core::provider::Provider;
use boitata_core::runtime;
use boitata_core::tools::{ToolPolicy, ToolRegistry};
use clap::Parser;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Parser)]
#[command(name = "boitata-agent")]
#[command(about = "Run the boitata agent as an ACP server", long_about = None)]
struct Args {
    /// Path to the config file (default: boitata.toml, or $BOITATA_CONFIG)
    #[arg(long)]
    config: Option<String>,
    /// Address to listen on for ACP clients
    #[arg(long, default_value = "127.0.0.1:9000")]
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

    // Same runtime assembly the CLI/server use; built once and shared across turns.
    runtime::init_workspace(&config);
    let provider = runtime::build_provider(&config)?;
    let tools = runtime::build_tools(&config).await?;
    let policy = runtime::build_policy(&config)?;

    let runner = Arc::new(AgentRunner {
        config: Arc::new(config),
        provider,
        tools,
        policy: Arc::new(policy),
    });

    let listener = TcpListener::bind(&args.addr)
        .await
        .with_context(|| format!("failed to bind {}", args.addr))?;
    info!("boitata-agent (ACP) listening on {}", args.addr);
    serve(listener, runner).await
}

/// Builds and runs a `boitata_agent::Agent` for each ACP prompt turn, wiring the
/// ACP-provided sink so every audit event streams back to the client.
struct AgentRunner {
    config: Arc<Config>,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    policy: Arc<ToolPolicy>,
}

#[async_trait]
impl PromptRunner for AgentRunner {
    async fn run(
        &self,
        prompt: String,
        sink: Arc<dyn AuditSink>,
        cancel: CancellationToken,
    ) -> anyhow::Result<AgentOutcome> {
        let cfg = &self.config;
        let mut agent = Agent::new(self.provider.clone(), self.tools.clone())
            .with_policy((*self.policy).clone())
            .with_audit(sink);
        if let Some(prompt) = cfg.system_prompt.clone() {
            agent = agent.with_system_prompt(prompt);
        }
        if let Some(max) = cfg.max_iterations {
            agent = agent.with_max_iterations(max);
        }
        if let Some(threshold) = cfg.auto_compact_threshold {
            agent = agent.with_compact_threshold(threshold);
        }

        let result = agent.run_with_cancel(Task::new(prompt), cancel).await?;
        Ok(AgentOutcome {
            success: result.success,
            message: result.final_message.or(result.error),
        })
    }
}
