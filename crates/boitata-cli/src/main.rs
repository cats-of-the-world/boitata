// Boitata: One-Shot Coding Agent
// Inspired by Stripe's Minions and Block's Goose

mod remote;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tracing::info;

use boitata_agent::{Agent, Task};
use boitata_core::audit::{self, FileAuditLog};
use boitata_core::config::Config;
use boitata_core::provider::Provider;
use boitata_core::runtime;
use boitata_core::tools::{ToolPolicy, ToolRegistry};
use boitata_orchestrator as blueprint;
use boitata_store::Store;

/// Default audit log path when the config doesn't set `audit_log`.
const DEFAULT_AUDIT_LOG: &str = "boitata-audit.log";

/// Default state-database path when the config doesn't set `state_db`.
const DEFAULT_STATE_DB: &str = "boitata.db";

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
    /// Resume an interrupted blueprint run from its last checkpoint
    Resume {
        /// The run id to resume (see `boitata runs`)
        run_id: String,
        /// Path to the same blueprint YAML file the run used
        #[arg(long)]
        blueprint: String,
        /// Path to the config file (default: boitata.toml, or $BOITATA_CONFIG)
        #[arg(long)]
        config: Option<String>,
    },
    /// List blueprint runs recorded in the state database
    Runs {
        /// Path to the config file (default: boitata.toml, or $BOITATA_CONFIG)
        #[arg(long)]
        config: Option<String>,
        /// Include finished runs too, not just the resumable ones
        #[arg(long)]
        all: bool,
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
            Some(url) => {
                // Resolve a bearer token for the remote server from the local
                // config (which checks BOITATA_API_TOKEN first) — but don't
                // require a config file just to talk to a remote server, so fall
                // back to the env var alone if there's no local config.
                let token = match Config::load(&Config::resolve_path(config)) {
                    Ok(cfg) => cfg.resolve_api_token(),
                    Err(_) => std::env::var("BOITATA_API_TOKEN")
                        .ok()
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty()),
                };
                remote::run(&url, task, blueprint, token).await
            }
            None => run_task(task, config, blueprint).await,
        },
        Commands::Resume {
            run_id,
            blueprint,
            config,
        } => resume_blueprint(run_id, blueprint, config).await,
        Commands::Runs { config, all } => list_runs(config, all).await,
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
    let audit = open_audit(&config, &run_id);

    // Confine the path tools, register the built-ins + MCP servers, and build the
    // permission policy — the same wiring the server uses (see `core::runtime`).
    runtime::init_workspace(&config);
    let tools = runtime::build_tools(&config).await?;
    let policy = runtime::build_policy(&config)?;

    // A blueprint runs a graph of agent/tool/script nodes; without one we run the
    // single-agent path (equivalent to a one-node agent blueprint).
    if let Some(name) = blueprint_name {
        return run_blueprint(&name, task, &config, provider, tools, audit, policy, run_id).await;
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
/// and agent settings the single-agent path uses. The run is checkpointed under
/// `run_id` so it can be resumed (see `boitata resume`) if interrupted.
#[allow(clippy::too_many_arguments)]
async fn run_blueprint(
    name: &str,
    task: String,
    config: &Config,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    audit: Option<Arc<audit::FileAuditLog>>,
    policy: ToolPolicy,
    run_id: String,
) -> anyhow::Result<()> {
    let graph = blueprint::load(name)?;
    let checkpointer = open_checkpointer(config)?;
    let executor =
        build_blueprint_executor(config, provider, tools, audit, policy, checkpointer, run_id);

    info!("Running blueprint `{name}` on task: {task}");
    let state = executor.run(&graph, task).await?;
    report_blueprint_state(name, &state)
}

/// Resume a previously-interrupted blueprint run from its persisted checkpoint.
/// Rebuilds the same execution environment as a fresh run and continues from the
/// last completed super-step.
async fn resume_blueprint(
    run_id: String,
    blueprint_path: String,
    config_path: Option<String>,
) -> anyhow::Result<()> {
    let path = Config::resolve_path(config_path);
    let config = Config::load(&path)?;
    let provider = runtime::build_provider(&config)?;
    let audit = open_audit(&config, &run_id);
    runtime::init_workspace(&config);
    let tools = runtime::build_tools(&config).await?;
    let policy = runtime::build_policy(&config)?;

    let graph = blueprint::load(&blueprint_path)?;
    let checkpointer = open_checkpointer(&config)?;
    let executor = build_blueprint_executor(
        &config,
        provider,
        tools,
        audit,
        policy,
        checkpointer,
        run_id,
    );

    let state = executor.resume(&graph).await?;
    report_blueprint_state(&blueprint_path, &state)
}

/// List blueprint runs recorded in the state database. Without `--all`, only
/// resumable (interrupted or crashed) runs are shown.
async fn list_runs(config_path: Option<String>, all: bool) -> anyhow::Result<()> {
    let path = Config::resolve_path(config_path);
    let config = Config::load(&path)?;
    let store = open_store(&config)?;
    let runs = store.list_checkpoints(!all).await?;

    if runs.is_empty() {
        println!("No {}runs recorded.", if all { "" } else { "resumable " });
        return Ok(());
    }

    println!(
        "{:<36}  {:<9}  {:>5}  {:<16}  TASK",
        "RUN ID", "STATUS", "STEP", "BLUEPRINT"
    );
    for r in runs {
        println!(
            "{:<36}  {:<9}  {:>5}  {:<16}  {}",
            r.run_id,
            r.status, // RunState: Display renders the lowercase label
            r.step,
            truncate(&r.blueprint, 16),
            truncate(&r.task, 60),
        );
    }
    if !all {
        println!("\nResume one with: boitata resume <RUN ID> --blueprint <path>");
    }
    Ok(())
}

/// Assemble the blueprint executor shared by the fresh-run and resume paths:
/// same provider/tools/policy/agent settings, plus the checkpointer and run id.
fn build_blueprint_executor(
    config: &Config,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    audit: Option<Arc<audit::FileAuditLog>>,
    policy: ToolPolicy,
    checkpointer: Arc<blueprint::SqliteCheckpointer>,
    run_id: String,
) -> blueprint::Executor {
    let mut executor = blueprint::Executor::new(provider, tools)
        .with_policy(policy)
        .with_system_prompt(config.system_prompt.clone())
        .with_max_iterations(config.max_iterations)
        .with_compact_threshold(config.auto_compact_threshold)
        // Forward the host's effective provider config into any sandbox a
        // `provision` node creates, so an in-container agent inherits it.
        .with_env_defaults(runtime::provider_env(config))
        .with_checkpointer(checkpointer)
        .with_run_id(run_id);
    if let Some(audit) = audit {
        executor = executor.with_audit(audit);
    }
    if let Some(max_steps) = config.blueprint_max_steps {
        executor = executor.with_max_steps(max_steps);
    }
    executor.with_max_retries(config.blueprint_max_retries)
}

/// Print a finished (or interrupted) blueprint's transcript, then map its status
/// to a process result. Writes ignore broken-pipe errors so piping into a command
/// that exits early can't panic the process (`println!` would).
fn report_blueprint_state(name: &str, state: &blueprint::State) -> anyhow::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "---");
    for (node, text) in state.transcript() {
        let _ = writeln!(out, "[{node}]\n{text}\n");
    }
    if matches!(state.status, Some(blueprint::Status::Ok)) {
        let _ = writeln!(out, "Blueprint `{name}` completed.");
    }
    match state.status {
        Some(blueprint::Status::Ok) => Ok(()),
        Some(blueprint::Status::Failed) => bail!("Blueprint `{name}` finished with a failing step"),
        None => bail!("Blueprint `{name}` finished with no node having run"),
    }
}

/// Open the SQLite state database at the configured path (default `boitata.db`).
fn open_store(config: &Config) -> anyhow::Result<Store> {
    let path = config
        .state_db
        .clone()
        .unwrap_or_else(|| DEFAULT_STATE_DB.to_string());
    Store::open(&path).with_context(|| format!("failed to open state database `{path}`"))
}

/// Open the state database and wrap it in a checkpointer for the executor.
fn open_checkpointer(config: &Config) -> anyhow::Result<Arc<blueprint::SqliteCheckpointer>> {
    Ok(Arc::new(blueprint::SqliteCheckpointer::new(open_store(
        config,
    )?)))
}

/// Open the JSONL audit log for a run, tagging events with `run_id`. A log we
/// can't open never aborts the run — losing the log is preferable to killing an
/// (often unattended) task — so we warn and continue without auditing.
fn open_audit(config: &Config, run_id: &str) -> Option<Arc<FileAuditLog>> {
    let audit_path = config
        .audit_log
        .clone()
        .unwrap_or_else(|| DEFAULT_AUDIT_LOG.to_string());
    match FileAuditLog::open(Path::new(&audit_path), run_id.to_string()) {
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
    }
}

/// Truncate `s` to `max` chars, appending `…` when shortened, for tidy columns.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
