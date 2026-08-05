//! Assembling a run-ready set of resources from a [`Config`].
//!
//! Both the CLI and the server need the same wiring — construct the provider,
//! register the built-in tools, connect MCP servers, build the permission
//! policy, and confine the path tools to a workspace. This module is the single
//! source of that logic so the two front-ends can't drift apart.

use std::sync::Arc;

use anyhow::{Context, bail};
use tracing::info;

use crate::config::{Config, McpServerConfig};
use crate::mcp::McpClient;
use crate::provider::{AnthropicProvider, OllamaProvider, OpenAIProvider, Provider};
use crate::tools::{
    CargoAddTool, CargoCheckTool, CargoClippyTool, CargoFmtTool, CargoTestTool, ExecuteCommandTool,
    FileEditTool, FileReadTool, FileWriteTool, GitBranchTool, GitCommitTool, GitDiffTool,
    GitStatusTool, ListDirectoryTool, SearchTool, ToolPolicy, ToolRegistry, workspace,
};

/// Fallback output-token budget when the config doesn't set `max_tokens`.
pub const DEFAULT_MAX_TOKENS: usize = 4096;

/// Construct a provider from config. API-key providers read the key from
/// `BOITATA_API_KEY` or the config file; Ollama needs no key.
pub fn build_provider(config: &Config) -> anyhow::Result<Arc<dyn Provider>> {
    let max_tokens = config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    // Resolve through the environment-aware accessors so `BOITATA_PROVIDER`,
    // `BOITATA_MODEL`, and `BOITATA_BASE_URL` (like `BOITATA_API_KEY`) can override
    // the file — the sandboxed agent is configured entirely by forwarded env vars.
    let provider_name = config.resolve_provider();
    let model = config.resolve_model();
    let base_url = config.resolve_base_url();

    let provider: Arc<dyn Provider> = match provider_name.as_str() {
        "anthropic" => {
            let api_key = config
                .resolve_api_key()
                .context("anthropic provider requires an api_key (config or BOITATA_API_KEY)")?;
            Arc::new(
                AnthropicProvider::with_config(api_key, model, base_url)
                    .with_max_tokens(max_tokens),
            )
        }
        "openai" => {
            let api_key = config
                .resolve_api_key()
                .context("openai provider requires an api_key (config or BOITATA_API_KEY)")?;
            Arc::new(
                OpenAIProvider::with_config(api_key, model, base_url).with_max_tokens(max_tokens),
            )
        }
        "ollama" => {
            Arc::new(OllamaProvider::with_config(model, base_url).with_max_tokens(max_tokens))
        }
        other => bail!("unknown provider `{other}` (expected: anthropic, openai, ollama)"),
    };

    Ok(provider)
}

/// The provider configuration as a map of `BOITATA_*` environment variables,
/// resolved from the environment or the config file. Handed to the blueprint
/// executor so a `provision` node can forward the host's effective config into a
/// sandbox — an in-container agent then inherits it without every value being
/// exported. **Includes the API key (a secret); never log this map.**
pub fn provider_env(config: &Config) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    env.insert("BOITATA_PROVIDER".to_string(), config.resolve_provider());
    env.insert("BOITATA_MODEL".to_string(), config.resolve_model());
    if let Some(base_url) = config.resolve_base_url() {
        env.insert("BOITATA_BASE_URL".to_string(), base_url);
    }
    if let Some(key) = config.resolve_api_key() {
        env.insert("BOITATA_API_KEY".to_string(), key);
    }
    env
}

/// Confine the path-taking tools to a workspace root. Secure by default:
/// confinement is on unless `confine_tools = false`, and the root defaults to the
/// current working directory when `workspace_root` is unset. Process-global —
/// call once at startup.
pub fn init_workspace(config: &Config) {
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
}

/// Register the deterministic built-in tools plus any configured MCP servers.
///
/// A `execute_command` tool is included unless `allow_execute_command = false`.
/// An MCP server we can't reach is logged and skipped so one broken server can't
/// abort startup; the registered tools keep their connections alive for the
/// life of the returned registry.
pub async fn build_tools(config: &Config) -> anyhow::Result<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    // File system
    tools.register(Arc::new(FileReadTool));
    tools.register(Arc::new(FileWriteTool));
    tools.register(Arc::new(FileEditTool));
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
    // Arbitrary shell execution is enabled by default so the agent is fully
    // capable out of the box; disable it for restricted deployments with
    // `allow_execute_command = false`.
    if config.allow_execute_command.unwrap_or(true) {
        tools.register(Arc::new(ExecuteCommandTool));
    } else {
        info!("execute_command tool disabled by config");
    }

    for server in &config.mcp_servers {
        match connect_mcp(server, &mut tools).await {
            Ok(count) => info!("MCP server `{}` connected: {count} tool(s)", server.name),
            Err(e) => tracing::warn!("MCP server `{}` unavailable: {e:#}", server.name),
        }
    }

    Ok(tools)
}

/// Build the tool permission policy from config. A bad denylist regex is a config
/// error we fail on rather than silently dropping a security control.
pub fn build_policy(config: &Config) -> anyhow::Result<ToolPolicy> {
    ToolPolicy::new(
        config.tool_policy.clone().unwrap_or_default(),
        &config.denied_commands,
    )
    .context("invalid `denied_commands` regex in config")
}

/// Connect to one MCP server and register its tools into `tools`. Returns the
/// number of tools discovered. The registered tools own the connection, keeping
/// the server alive for the duration of the run.
async fn connect_mcp(server: &McpServerConfig, tools: &mut ToolRegistry) -> anyhow::Result<usize> {
    let client = McpClient::connect(server).await?;
    let mcp_tools = client.discover_tools().await?;
    // Count only tools that actually registered: a namespaced name can collide
    // with a built-in or another server's tool, in which case `register` keeps
    // the existing one and skips the duplicate (see `ToolRegistry::register`).
    let mut count = 0;
    for tool in mcp_tools {
        if tools.register(tool) {
            count += 1;
        }
    }
    Ok(count)
}
