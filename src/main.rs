// Boitata: One-Shot Coding Agent
// Inspired by Stripe's Minions and Block's Goose

// Declare modules
mod agent;
mod audit;
mod config;
mod context;
mod mcp;
mod provider;
mod tools;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use agent::{Agent, Task};
use audit::FileAuditLog;
use config::{Config, McpServerConfig};
use mcp::McpClient;
use provider::{AnthropicProvider, OllamaProvider, OpenAIProvider, Provider};
use tools::workspace;
use tools::{
    CargoAddTool, CargoCheckTool, CargoClippyTool, CargoFmtTool, CargoTestTool, ExecuteCommandTool,
    FileReadTool, FileWriteTool, GitBranchTool, GitCommitTool, GitDiffTool, GitStatusTool,
    ListDirectoryTool, SearchTool, ToolRegistry,
};

/// Fallback output-token budget when the config doesn't set `max_tokens`.
const DEFAULT_MAX_TOKENS: usize = 4096;
/// Default audit log path when the config doesn't set `audit_log`.
const DEFAULT_AUDIT_LOG: &str = "boitata-audit.log";

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

    // Set up the audit log for this run. A log we can't open must never abort
    // the run — losing the log is preferable to killing an (often unattended)
    // task, so we warn and continue without auditing.
    let run_id = uuid::Uuid::new_v4().to_string();
    let audit_path = config
        .audit_log
        .clone()
        .unwrap_or_else(|| DEFAULT_AUDIT_LOG.to_string());
    let audit = match FileAuditLog::open(Path::new(&audit_path), run_id.clone()) {
        Ok(audit) => {
            info!("Audit log: {audit_path} (run_id={run_id})");
            Some(Arc::new(audit))
        }
        Err(e) => {
            tracing::warn!(
                "failed to open audit log `{audit_path}`: {e}; continuing without audit"
            );
            None
        }
    };

    // Confine the path-taking tools to a workspace root. Secure by default:
    // confinement is on unless `confine_tools = false`, and the root defaults to
    // the current working directory when `workspace_root` is unset.
    let workspace_root = if config.confine_tools.unwrap_or(true) {
        let root = config
            .workspace_root
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        info!("Confining path tools to workspace root: {}", root.display());
        Some(root)
    } else {
        info!("Tool path confinement disabled (confine_tools = false)");
        None
    };
    workspace::init(workspace_root);

    // Register the deterministic built-in tools the agent can call.
    let mut tools = ToolRegistry::new();
    // File system
    tools.register(Arc::new(FileReadTool));
    tools.register(Arc::new(FileWriteTool));
    tools.register(Arc::new(ListDirectoryTool));
    // Search
    tools.register(Arc::new(SearchTool));
    // Git
    tools.register(Arc::new(GitStatusTool));
    tools.register(Arc::new(GitDiffTool));
    tools.register(Arc::new(GitCommitTool));
    tools.register(Arc::new(GitBranchTool));
    // Cargo
    tools.register(Arc::new(CargoCheckTool));
    tools.register(Arc::new(CargoClippyTool));
    tools.register(Arc::new(CargoFmtTool));
    tools.register(Arc::new(CargoTestTool));
    tools.register(Arc::new(CargoAddTool));
    // Arbitrary command execution — opt-out via config for restricted deployments.
    if config.allow_execute_command.unwrap_or(true) {
        tools.register(Arc::new(ExecuteCommandTool));
    } else {
        info!("execute_command tool disabled by config");
    }

    // Connect any configured MCP servers and register their tools. A server we
    // can't reach is logged and skipped so one broken server can't abort the
    // run. The registered tools keep the connections alive for the run.
    for server in &config.mcp_servers {
        match connect_mcp(server, &mut tools).await {
            Ok(count) => info!("MCP server `{}` connected: {count} tool(s)", server.name),
            Err(e) => warn!("MCP server `{}` unavailable: {e:#}", server.name),
        }
    }

    let mut agent = Agent::new(provider, tools);
    if let Some(audit) = audit {
        agent = agent.with_audit(audit);
    }
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

/// Connect to one MCP server and register its tools into `tools`. Returns the
/// number of tools discovered. The registered tools own the connection, keeping
/// the server alive for the duration of the run.
async fn connect_mcp(server: &McpServerConfig, tools: &mut ToolRegistry) -> anyhow::Result<usize> {
    let client = McpClient::connect(server).await?;
    let mcp_tools = client.discover_tools().await?;
    let count = mcp_tools.len();
    for tool in mcp_tools {
        tools.register(tool);
    }
    Ok(count)
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
