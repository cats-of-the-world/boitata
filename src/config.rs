// Configuration loading for Boitata.
//
// Configuration lives in a TOML file (default: `boitata.toml` in the current
// directory) so credentials never need to be passed on the command line. The
// file is git-ignored; commit `boitata.example.toml` as a template instead.

use anyhow::{Context, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Environment variable holding an alternate config file path.
const CONFIG_PATH_ENV: &str = "BOITATA_CONFIG";
/// Environment variable overriding the API key (takes precedence over the file).
const API_KEY_ENV: &str = "BOITATA_API_KEY";
/// Default config file name, looked up in the current directory.
const DEFAULT_CONFIG_FILE: &str = "boitata.toml";

/// Top-level Boitata configuration.
///
/// `Debug` is implemented manually to redact the API key — never derive it, or
/// the secret leaks anywhere the config is logged or formatted.
#[derive(Clone, Deserialize)]
pub struct Config {
    /// Provider to use: `anthropic`, `openai`, or `ollama`.
    pub provider: String,
    /// Model identifier passed to the provider (e.g. `glm-4.6`).
    pub model: String,
    /// API key. May be omitted here and supplied via `BOITATA_API_KEY` instead.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Override the provider's default endpoint (e.g. an OpenAI-compatible proxy).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Output token budget per request. Defaults to a conservative value.
    #[serde(default)]
    pub max_tokens: Option<usize>,
    /// Maximum agent iterations before giving up.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// Optional custom system prompt for the agent.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Path to the JSONL audit log (optional; defaults to `boitata-audit.log`).
    #[serde(default)]
    pub audit_log: Option<String>,
    /// MCP servers to connect to. Their tools are exposed to the agent.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Whether to register the arbitrary `execute_command` tool. Defaults to
    /// enabled; set to `false` to restrict the agent to the structured tools.
    #[serde(default)]
    pub allow_execute_command: Option<bool>,
    /// Directory that path-taking tools (file_read/write, list_directory,
    /// search) are confined to. Defaults to the current working directory.
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// Whether to confine path-taking tools to `workspace_root`. Secure by
    /// default (`true`); set to `false` to let them access any path.
    #[serde(default)]
    pub confine_tools: Option<bool>,
    /// Tool permission policy: `allow_all` (default) or `read_only` (deny any
    /// tool that may modify state).
    #[serde(default)]
    pub tool_policy: Option<crate::tools::PolicyMode>,
    /// Regex patterns; an `execute_command` whose command matches any of these
    /// is denied by the policy.
    #[serde(default)]
    pub denied_commands: Vec<String>,
    /// Fraction of the model's context window (0.0–1.0) at which older turns are
    /// summarized to avoid overflow. Defaults to `0.8`; set to `0.0` to disable
    /// compaction.
    #[serde(default)]
    pub auto_compact_threshold: Option<f32>,
    /// Cap on how many nodes a blueprint run may execute (bounds cyclic graphs).
    /// Only used with `--blueprint`; defaults to the executor's built-in limit.
    #[serde(default)]
    pub blueprint_max_steps: Option<usize>,
}

/// A single MCP server. The transport is inferred from which field is set:
/// `command` → stdio (subprocess), `url` → Streamable HTTP (remote). Exactly one
/// of the two must be present; [`McpServerConfig::transport`] validates this and
/// projects the flat config into the [`McpTransport`] enum.
///
/// `Debug` is implemented manually to redact `auth_token` and the values of
/// `env`/`headers`, which routinely carry credentials.
#[derive(Clone, Default, Deserialize)]
pub struct McpServerConfig {
    /// Short identifier used to namespace this server's tools (e.g. `git`).
    pub name: String,

    // --- stdio transport ---
    /// Executable to run (e.g. `npx`, `uvx`, an absolute path).
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,

    // --- Streamable HTTP transport ---
    /// URL of a remote MCP server (Streamable HTTP endpoint).
    #[serde(default)]
    pub url: Option<String>,
    /// Bearer token sent as `Authorization: Bearer <token>` (no prefix here).
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Extra HTTP headers to send with every request.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// A validated MCP transport borrowed from an [`McpServerConfig`]: exactly one
/// of stdio or HTTP. Constructed only via [`McpServerConfig::transport`], so
/// downstream code (e.g. `McpClient::connect`) never handles the "both" or
/// "neither" states.
pub enum McpTransport<'a> {
    Stdio {
        command: &'a str,
        args: &'a [String],
        env: &'a HashMap<String, String>,
    },
    Http {
        url: &'a str,
        auth_token: Option<&'a str>,
        headers: &'a HashMap<String, String>,
    },
}

impl McpServerConfig {
    /// Validate the transport fields and project into [`McpTransport`]. Errors if
    /// neither or both of `command`/`url` are set.
    pub fn transport(&self) -> anyhow::Result<McpTransport<'_>> {
        match (&self.url, &self.command) {
            (Some(_), Some(_)) => bail!(
                "MCP server `{}` sets both `url` and `command`; use exactly one",
                self.name
            ),
            (Some(url), None) => Ok(McpTransport::Http {
                url,
                auth_token: self.auth_token.as_deref(),
                headers: &self.headers,
            }),
            (None, Some(command)) => Ok(McpTransport::Stdio {
                command,
                args: &self.args,
                env: &self.env,
            }),
            (None, None) => bail!(
                "MCP server `{}` must set either `command` (stdio) or `url` (http)",
                self.name
            ),
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("provider", &self.provider)
            .field("model", &self.model)
            // Redacted: only whether a key is present, never its value.
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("base_url", &self.base_url)
            .field("max_tokens", &self.max_tokens)
            .field("max_iterations", &self.max_iterations)
            .field("system_prompt", &self.system_prompt)
            .field("audit_log", &self.audit_log)
            .field("mcp_servers", &self.mcp_servers)
            .field("allow_execute_command", &self.allow_execute_command)
            .field("workspace_root", &self.workspace_root)
            .field("confine_tools", &self.confine_tools)
            .field("tool_policy", &self.tool_policy)
            .field("denied_commands", &self.denied_commands)
            .finish()
    }
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show keys but redact values for maps that may hold credentials.
        let redact = |map: &HashMap<String, String>| -> BTreeMap<String, &'static str> {
            map.keys().map(|k| (k.clone(), "***")).collect()
        };
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &redact(&self.env))
            .field("url", &self.url)
            .field("auth_token", &self.auth_token.as_ref().map(|_| "***"))
            .field("headers", &redact(&self.headers))
            .finish()
    }
}

impl Config {
    /// Resolve the config file path from an explicit CLI argument, then the
    /// `BOITATA_CONFIG` env var, then the default `boitata.toml`.
    pub fn resolve_path(explicit: Option<String>) -> PathBuf {
        explicit
            .or_else(|| std::env::var(CONFIG_PATH_ENV).ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE))
    }

    /// Load and parse the config file at `path`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read config file `{}` (copy boitata.example.toml to get started)",
                path.display()
            )
        })?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse config file `{}`", path.display()))
    }

    /// The API key to use: `BOITATA_API_KEY` if set, otherwise the file value.
    pub fn resolve_api_key(&self) -> Option<String> {
        // Treat a blank key (common in the committed template) as absent so the
        // caller surfaces the actionable "requires an api_key" error rather than
        // sending an empty `Authorization` header and getting a confusing 401.
        std::env::var(API_KEY_ENV)
            .ok()
            .or_else(|| self.api_key.clone())
            .filter(|k| !k.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let config: Config = toml::from_str(
            r#"
            provider = "openai"
            model = "glm-4.6"
            base_url = "https://api.z.ai/api/paas/v4/chat/completions"
            "#,
        )
        .unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "glm-4.6");
        assert_eq!(config.api_key, None);
        assert_eq!(config.max_tokens, None);
    }

    #[test]
    fn test_resolve_path_default() {
        // No explicit arg and (assuming) no env var → default file name.
        unsafe {
            std::env::remove_var(CONFIG_PATH_ENV);
        }
        assert_eq!(
            Config::resolve_path(None),
            PathBuf::from(DEFAULT_CONFIG_FILE)
        );
        assert_eq!(
            Config::resolve_path(Some("custom.toml".to_string())),
            PathBuf::from("custom.toml")
        );
    }

    #[test]
    fn test_transport_infers_stdio_and_http() {
        let stdio = McpServerConfig {
            name: "x".to_string(),
            command: Some("npx".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            stdio.transport().unwrap(),
            McpTransport::Stdio { .. }
        ));

        let http = McpServerConfig {
            name: "x".to_string(),
            url: Some("https://h/mcp".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            http.transport().unwrap(),
            McpTransport::Http { .. }
        ));
    }

    #[test]
    fn test_transport_requires_exactly_one() {
        let neither = McpServerConfig {
            name: "x".to_string(),
            ..Default::default()
        };
        let err = neither.transport().err().unwrap().to_string();
        assert!(err.contains("command") && err.contains("url"), "{err}");

        let both = McpServerConfig {
            name: "x".to_string(),
            command: Some("true".to_string()),
            url: Some("https://h/mcp".to_string()),
            ..Default::default()
        };
        let err = both.transport().err().unwrap().to_string();
        assert!(err.contains("both"), "{err}");
    }

    #[test]
    fn test_debug_redacts_secrets() {
        let config = Config {
            provider: "openai".to_string(),
            model: "glm-4.6".to_string(),
            api_key: Some("super-secret-key".to_string()),
            base_url: None,
            max_tokens: None,
            max_iterations: None,
            system_prompt: None,
            audit_log: None,
            mcp_servers: vec![McpServerConfig {
                name: "remote".to_string(),
                url: Some("https://h/mcp".to_string()),
                auth_token: Some("super-secret-token".to_string()),
                headers: HashMap::from([("X-Api-Key".to_string(), "hdr-secret".to_string())]),
                ..Default::default()
            }],
            allow_execute_command: None,
            workspace_root: None,
            confine_tools: None,
            tool_policy: None,
            denied_commands: Vec::new(),
            auto_compact_threshold: None,
            blueprint_max_steps: None,
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(!rendered.contains("hdr-secret"), "{rendered}");
        // Non-secret metadata is still visible for debugging.
        assert!(rendered.contains("X-Api-Key"), "{rendered}");
        assert!(rendered.contains("glm-4.6"), "{rendered}");
    }
}
