// Boitata: One-Shot Coding Agent
// Inspired by Stripe's Minions and Block's Goose

mod remote;

use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use clap::{Parser, Subcommand};
use tracing::info;

use boitata_agent::{Agent, Task};
use boitata_core::audit::{self, FileAuditLog};
use boitata_core::config::Config;
use boitata_core::provider::Provider;
use boitata_core::runtime;
use boitata_core::tools::{ToolPolicy, ToolRegistry};
use boitata_orchestrator as blueprint;

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
        /// Path to a blueprint YAML file to run (see examples/blueprints/ for
        /// ready-to-copy starting points)
        #[arg(long)]
        blueprint: Option<String>,
        /// Schedule the task on a running boitata-server and stream its progress,
        /// e.g. --remote http://127.0.0.1:8787 (instead of running locally)
        #[arg(long)]
        remote: Option<String>,
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
            remote,
        } => match remote {
            Some(url) => remote::run(&url, task, blueprint).await,
            None => run_task(task, config, blueprint).await,
        },
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
    blueprint_name: Option<String>,
) -> anyhow::Result<()> {
    let path = Config::resolve_path(config_path);
    let config = Config::load(&path)?;
    info!(
        "Loaded config from {} (provider={}, model={})",
        path.display(),
        config.provider,
        config.model
    );

    let provider = runtime::build_provider(&config)?;

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

    // Confine the path tools, register the built-ins + MCP servers, and build the
    // permission policy — the same wiring the server uses (see `core::runtime`).
    runtime::init_workspace(&config);
    let tools = runtime::build_tools(&config).await?;
    let policy = runtime::build_policy(&config)?;

    // A blueprint runs a graph of agent/tool/script nodes; without one we run the
    // single-agent path (equivalent to a one-node agent blueprint).
    if let Some(name) = blueprint_name {
        return run_blueprint(&name, task, &config, provider, tools, audit, policy).await;
    }

    let mut agent = Agent::new(provider, tools).with_policy(policy);
    if let Some(audit) = audit {
        agent = agent.with_audit(audit);
    }
    if let Some(prompt) = config.system_prompt.clone() {
        agent = agent.with_system_prompt(prompt);
    }
    if let Some(max_iterations) = config.max_iterations {
        agent = agent.with_max_iterations(max_iterations);
    }
    if let Some(threshold) = config.auto_compact_threshold {
        agent = agent.with_compact_threshold(threshold);
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

/// Run a named blueprint. Agent nodes inherit the same provider, tools, policy,
/// and agent settings the single-agent path uses.
async fn run_blueprint(
    name: &str,
    task: String,
    config: &Config,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    audit: Option<Arc<audit::FileAuditLog>>,
    policy: ToolPolicy,
) -> anyhow::Result<()> {
    let graph = blueprint::load(name)?;

    let mut executor = blueprint::Executor::new(provider, tools)
        .with_policy(policy)
        .with_system_prompt(config.system_prompt.clone())
        .with_max_iterations(config.max_iterations)
        .with_compact_threshold(config.auto_compact_threshold)
        // Forward the host's effective provider config into any sandbox a
        // `provision` node creates, so an in-container agent inherits it.
        .with_env_defaults(runtime::provider_env(config));
    if let Some(audit) = audit {
        executor = executor.with_audit(audit);
    }
    if let Some(max_steps) = config.blueprint_max_steps {
        executor = executor.with_max_steps(max_steps);
    }
    executor = executor.with_max_retries(config.blueprint_max_retries);

    info!("Running blueprint `{name}` on task: {task}");
    let state = executor.run(&graph, task).await?;

    // Write the transcript and result directly, ignoring write errors: a broken
    // pipe (output piped into a command that exits early) must not panic the
    // process. `println!` would panic on that, so use `writeln!`.
    {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "---");
        for (node, text) in state.transcript() {
            let _ = writeln!(out, "[{node}]\n{text}\n");
        }
        if matches!(state.status, Some(blueprint::Status::Ok)) {
            let _ = writeln!(out, "Blueprint `{name}` completed.");
        }
    }
    match state.status {
        Some(blueprint::Status::Ok) => Ok(()),
        Some(blueprint::Status::Failed) => {
            bail!("Blueprint `{name}` finished with a failing step");
        }
        None => bail!("Blueprint `{name}` finished with no node having run"),
    }
}
