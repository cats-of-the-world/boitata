// Boitata: One-Shot Coding Agent
// Inspired by Stripe's Minions and Block's Goose

// Declare modules
mod agent;
mod config;
mod context;
mod provider;
mod tools;

use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tracing::info;

use agent::{Agent, Task};
use config::Config;
use provider::{AnthropicProvider, OllamaProvider, OpenAIProvider, Provider};
use tools::{FileReadTool, FileWriteTool, ListDirectoryTool, ToolRegistry};

/// Fallback output-token budget when the config doesn't set `max_tokens`.
const DEFAULT_MAX_TOKENS: usize = 4096;

#[derive(Parser)]
#[command(name = "boitata")]
#[command(about = "One-shot coding agent", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a task
    Run {
        /// The task description
        task: String,
        /// Path to the config file (default: boitata.toml, or $BOITATA_CONFIG)
        #[arg(long)]
        config: Option<String>,
        /// Use a specific blueprint
        #[arg(long)]
        blueprint: Option<String>,
    },
    /// Create a new task
    TaskCreate {
        /// Task description
        description: String,
    },
    /// List all tasks
    TaskList,
    /// Create a new workspace
    WorkspaceCreate {
        /// Path for the workspace
        path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            task,
            config,
            blueprint,
        } => run_task(task, config, blueprint).await,
        Commands::TaskCreate { description } => {
            info!("Creating task: {}", description);
            println!("Task creation not yet implemented");
            Ok(())
        }
        Commands::TaskList => {
            info!("Listing tasks");
            println!("Task listing not yet implemented");
            Ok(())
        }
        Commands::WorkspaceCreate { path } => {
            info!("Creating workspace at: {}", path);
            println!("Workspace creation not yet implemented");
            Ok(())
        }
    }
}

/// Load config, build the provider and tools, and run the agent on `task`.
async fn run_task(
    task: String,
    config_path: Option<String>,
    blueprint: Option<String>,
) -> anyhow::Result<()> {
    if let Some(bp) = blueprint {
        info!("Ignoring blueprint (not yet implemented): {}", bp);
    }

    let path = Config::resolve_path(config_path);
    let config = Config::load(&path)?;
    info!(
        "Loaded config from {} (provider={}, model={})",
        path.display(),
        config.provider,
        config.model
    );

    let provider = build_provider(&config)?;

    // Register the deterministic built-in tools the agent can call.
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FileReadTool));
    tools.register(Arc::new(FileWriteTool));
    tools.register(Arc::new(ListDirectoryTool));

    let mut agent = Agent::new(provider, tools);
    if let Some(prompt) = config.system_prompt.clone() {
        agent = agent.with_system_prompt(prompt);
    }
    if let Some(max_iterations) = config.max_iterations {
        agent = agent.with_max_iterations(max_iterations);
    }

    info!("Running task: {}", task);
    let result = agent.run(Task::new(task)).await?;

    // Report what happened.
    for call in &result.tool_calls {
        let marker = if call.is_error { "✗" } else { "✓" };
        println!("{marker} {}({})", call.name, call.arguments);
    }
    println!("---");
    if result.success {
        println!("Task completed in {} iteration(s).", result.iterations);
        if let Some(message) = result.final_message {
            println!("\n{message}");
        }
    } else {
        bail!(
            "Task did not complete: {}",
            result.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }

    Ok(())
}

/// Construct a provider from config. API-key providers read the key from
/// `BOITATA_API_KEY` or the config file; Ollama needs no key.
fn build_provider(config: &Config) -> anyhow::Result<Arc<dyn Provider>> {
    let max_tokens = config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let provider: Arc<dyn Provider> = match config.provider.as_str() {
        "anthropic" => {
            let api_key = config
                .resolve_api_key()
                .context("anthropic provider requires an api_key (config or BOITATA_API_KEY)")?;
            Arc::new(
                AnthropicProvider::with_config(
                    api_key,
                    config.model.clone(),
                    config.base_url.clone(),
                )
                .with_max_tokens(max_tokens),
            )
        }
        "openai" => {
            let api_key = config
                .resolve_api_key()
                .context("openai provider requires an api_key (config or BOITATA_API_KEY)")?;
            Arc::new(
                OpenAIProvider::with_config(api_key, config.model.clone(), config.base_url.clone())
                    .with_max_tokens(max_tokens),
            )
        }
        "ollama" => Arc::new(
            OllamaProvider::with_config(config.model.clone(), config.base_url.clone())
                .with_max_tokens(max_tokens),
        ),
        other => bail!("unknown provider `{other}` (expected: anthropic, openai, ollama)"),
    };

    Ok(provider)
}
