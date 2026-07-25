// MCP (Model Context Protocol) integration.
//
// Connects to MCP servers using the official `rmcp` client, discovers their
// tools, and adapts each one into a [`crate::tools::Tool`] so the agent can call
// it exactly like a built-in. Because MCP tools flow through the normal agent
// loop, their calls are captured by the audit log automatically.
//
// Two transports are supported, inferred from the server config: `command` for
// stdio (spawned subprocess) and `url` for Streamable HTTP (remote server).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
    transport::{
        ConfigureCommandExt, TokioChildProcess,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    },
};
use serde_json::Value;
use tokio::process::Command;

use crate::config::McpServerConfig;
use crate::tools::{Tool, ToolError};

/// Maximum length of a tool name exposed to the model (provider schemas cap
/// names at 64 chars and restrict the character set).
const MAX_TOOL_NAME_LEN: usize = 64;

/// A live connection to a single MCP server.
///
/// Holds the running `rmcp` service; dropping it shuts the server down and
/// cleans up the child process. The connection is kept alive by the tools it
/// produces (each [`McpTool`] holds an `Arc<McpClient>`), so it lives exactly as
/// long as its tools remain registered.
pub struct McpClient {
    name: String,
    service: RunningService<RoleClient, ()>,
}

impl McpClient {
    /// Connect to the configured MCP server and perform the initialize
    /// handshake. The transport is inferred: `url` → Streamable HTTP, `command`
    /// → stdio subprocess. Exactly one must be set.
    ///
    /// `()` is the no-op client handler; `.serve` runs the handshake. Both
    /// transports yield the same `RunningService`, so the caller is transport-
    /// agnostic afterward.
    pub async fn connect(config: &McpServerConfig) -> anyhow::Result<Arc<Self>> {
        let service = match (&config.url, &config.command) {
            (Some(_), Some(_)) => bail!(
                "MCP server `{}` sets both `url` and `command`; use exactly one",
                config.name
            ),
            (Some(url), None) => {
                // Build via `from_config` without naming the concrete HTTP client
                // type: rmcp bundles its own reqwest version, distinct from ours.
                let transport =
                    StreamableHttpClientTransport::from_config(build_http_config(url, config)?);
                ().serve(transport).await.with_context(|| {
                    format!("MCP handshake failed for `{}` ({url})", config.name)
                })?
            }
            (None, Some(command)) => {
                let transport = build_stdio_transport(command, config)?;
                ().serve(transport)
                    .await
                    .with_context(|| format!("MCP handshake failed for `{}`", config.name))?
            }
            (None, None) => bail!(
                "MCP server `{}` must set either `command` (stdio) or `url` (http)",
                config.name
            ),
        };

        Ok(Arc::new(Self {
            name: config.name.clone(),
            service,
        }))
    }

    /// Discover the server's tools and wrap each as a registrable [`Tool`].
    pub async fn discover_tools(self: &Arc<Self>) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .with_context(|| format!("tool discovery failed for `{}`", self.name))?;

        let mut out: Vec<Arc<dyn Tool>> = Vec::with_capacity(tools.len());
        for tool in tools {
            let remote_name = tool.name.to_string();
            // Namespace with the server name to avoid collisions across servers
            // and with built-in tools.
            let exposed_name = sanitize_tool_name(&format!("{}_{}", self.name, remote_name));
            let description = tool.description.map(|d| d.to_string()).unwrap_or_default();
            let input_schema = Value::Object((*tool.input_schema).clone());

            out.push(Arc::new(McpTool {
                client: Arc::clone(self),
                remote_name,
                exposed_name,
                description,
                input_schema,
            }));
        }
        Ok(out)
    }

    /// Invoke a tool by its server-side name and return the text result.
    async fn call(&self, remote_name: &str, arguments: Value) -> anyhow::Result<String> {
        let mut params = CallToolRequestParams::new(remote_name.to_string());
        if let Some(object) = arguments.as_object() {
            params = params.with_arguments(object.clone());
        }

        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| anyhow!("MCP tool `{remote_name}` call failed: {e}"))?;

        let text = result_text(&result);
        if result.is_error.unwrap_or(false) {
            return Err(anyhow!(if text.is_empty() {
                format!("MCP tool `{remote_name}` reported an error")
            } else {
                text
            }));
        }
        Ok(text)
    }
}

/// Build a stdio transport that spawns the server as a subprocess.
fn build_stdio_transport(
    command: &str,
    config: &McpServerConfig,
) -> anyhow::Result<TokioChildProcess> {
    let command = Command::new(command).configure(|cmd| {
        cmd.args(&config.args);
        cmd.envs(&config.env);
    });
    TokioChildProcess::new(command).with_context(|| {
        format!(
            "failed to spawn MCP server `{}` (command: {})",
            config.name,
            config.command.as_deref().unwrap_or_default()
        )
    })
}

/// Build the Streamable HTTP transport config for a remote server, applying the
/// optional bearer token and any custom headers. Returns the config (not the
/// transport) so the reqwest-typed transport is only ever constructed via
/// `from_config`, never named — rmcp bundles a different reqwest version.
fn build_http_config(
    url: &str,
    config: &McpServerConfig,
) -> anyhow::Result<StreamableHttpClientTransportConfig> {
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.to_string());

    if let Some(token) = &config.auth_token {
        // rmcp adds the `Bearer ` prefix itself.
        transport_config = transport_config.auth_header(token.clone());
    }

    if !config.headers.is_empty() {
        let mut headers = HashMap::with_capacity(config.headers.len());
        for (key, value) in &config.headers {
            let name = HeaderName::from_bytes(key.as_bytes())
                .with_context(|| format!("invalid HTTP header name `{key}`"))?;
            let value = HeaderValue::from_str(value)
                .with_context(|| format!("invalid HTTP header value for `{key}`"))?;
            headers.insert(name, value);
        }
        transport_config = transport_config.custom_headers(headers);
    }

    Ok(transport_config)
}

/// A single MCP tool adapted to the agent's [`Tool`] trait.
struct McpTool {
    client: Arc<McpClient>,
    /// Name as the server knows it (used in `tools/call`).
    remote_name: String,
    /// Namespaced name exposed to the model.
    exposed_name: String,
    description: String,
    input_schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, arguments: Value) -> crate::tools::Result<String> {
        self.client
            .call(&self.remote_name, arguments)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
    }
}

/// Flatten an MCP tool result into a single string for the agent. Text blocks
/// are concatenated; non-text blocks are noted; empty results fall back to any
/// structured content.
fn result_text(result: &CallToolResult) -> String {
    let mut parts = Vec::new();
    for block in &result.content {
        match block.as_text() {
            Some(text) => parts.push(text.text.clone()),
            None => parts.push("[non-text content omitted]".to_string()),
        }
    }
    let text = parts.join("\n");
    if text.is_empty() {
        if let Some(structured) = &result.structured_content {
            return structured.to_string();
        }
    }
    text
}

/// Restrict a tool name to `[A-Za-z0-9_-]` and cap its length so it satisfies
/// provider function-name constraints.
fn sanitize_tool_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.truncate(MAX_TOOL_NAME_LEN);
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_replaces_invalid_chars() {
        assert_eq!(sanitize_tool_name("git.read file"), "git_read_file");
        assert_eq!(sanitize_tool_name("fs_read-file"), "fs_read-file");
    }

    #[test]
    fn test_sanitize_truncates() {
        let long = "a".repeat(100);
        assert_eq!(sanitize_tool_name(&long).len(), MAX_TOOL_NAME_LEN);
    }

    #[test]
    fn test_result_text_joins_text_blocks() {
        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"}
            ]
        }))
        .unwrap();
        assert_eq!(result_text(&result), "hello\nworld");
    }

    #[test]
    fn test_result_text_falls_back_to_structured() {
        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [],
            "structuredContent": {"ok": true}
        }))
        .unwrap();
        assert_eq!(result_text(&result), r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn test_connect_requires_a_transport() {
        let config = McpServerConfig {
            name: "x".to_string(),
            ..Default::default()
        };
        let err = McpClient::connect(&config)
            .await
            .err()
            .expect("expected an error")
            .to_string();
        assert!(err.contains("command") && err.contains("url"), "{err}");
    }

    #[tokio::test]
    async fn test_connect_rejects_both_transports() {
        let config = McpServerConfig {
            name: "x".to_string(),
            command: Some("true".to_string()),
            url: Some("http://localhost/mcp".to_string()),
            ..Default::default()
        };
        let err = McpClient::connect(&config)
            .await
            .err()
            .expect("expected an error")
            .to_string();
        assert!(err.contains("both"), "{err}");
    }

    #[test]
    fn test_http_config_applies_auth_token() {
        let config = McpServerConfig {
            name: "x".to_string(),
            auth_token: Some("secret".to_string()),
            ..Default::default()
        };
        let cfg = build_http_config("http://localhost/mcp", &config).unwrap();
        assert_eq!(cfg.auth_header.as_deref(), Some("secret"));
    }

    #[test]
    fn test_http_config_rejects_invalid_header() {
        let mut headers = HashMap::new();
        headers.insert("bad header".to_string(), "v".to_string());
        let config = McpServerConfig {
            name: "x".to_string(),
            headers,
            ..Default::default()
        };
        assert!(build_http_config("http://localhost/mcp", &config).is_err());
    }

    /// A minimal MCP stdio server (newline-delimited JSON-RPC) implemented in
    /// Rust. Reads requests from stdin and writes responses to stdout.
    fn run_stdio_test_server() {
        use std::io::{BufRead, Write};

        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut stdout = std::io::stdout();
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF or read error: the client went away.
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

            let response = match method {
                "initialize" => Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": req["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "boitata-test", "version": "0.0.1"},
                    }
                })),
                "notifications/initialized" => None,
                "tools/list" => Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"tools": [{
                        "name": "echo",
                        "description": "Echo text back",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    }]}
                })),
                "tools/call" => {
                    let text = req["params"]["arguments"]["text"].as_str().unwrap_or("");
                    Some(serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "content": [{"type": "text", "text": format!("echo: {text}")}],
                            "isError": false,
                        }
                    }))
                }
                _ if !id.is_null() => Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"}
                })),
                _ => None,
            };

            if let Some(response) = response {
                let _ = writeln!(stdout, "{}", serde_json::to_string(&response).unwrap());
                let _ = stdout.flush();
            }
        }
    }

    /// Server-mode entry point. When `BOITATA_MCP_TEST_SERVER` is set, this
    /// process serves MCP over stdio instead of behaving as a test; the
    /// end-to-end test re-executes the test binary in this mode. Marked
    /// `#[ignore]` so the normal suite never runs it directly.
    #[test]
    #[ignore = "re-executed as a subprocess by test_stdio_server_end_to_end"]
    fn mcp_stdio_test_server() {
        if std::env::var_os("BOITATA_MCP_TEST_SERVER").is_none() {
            return;
        }
        run_stdio_test_server();
        std::process::exit(0);
    }

    #[tokio::test]
    async fn test_stdio_server_end_to_end() {
        // Drive the real rmcp client against a Rust MCP server with no external
        // runtime: re-execute this test binary in server mode (see
        // `mcp_stdio_test_server`). Passing `--ignored --nocapture` runs only
        // that test and lets its stdout reach our pipe; rmcp tolerates libtest's
        // non-JSON framing lines.
        let exe = std::env::current_exe().expect("current exe");
        let mut env = HashMap::new();
        env.insert("BOITATA_MCP_TEST_SERVER".to_string(), "1".to_string());

        let config = McpServerConfig {
            name: "test".to_string(),
            command: Some(exe.to_string_lossy().into_owned()),
            args: vec![
                "mcp_stdio_test_server".to_string(),
                "--ignored".to_string(),
                "--nocapture".to_string(),
            ],
            env,
            ..Default::default()
        };

        let client = McpClient::connect(&config).await.expect("connect");
        let tools = client.discover_tools().await.expect("discover");

        // Tool is discovered and namespaced with the server name.
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "test_echo");

        // Calling it round-trips through rmcp and returns the server's output.
        let out = tools[0]
            .execute(serde_json::json!({"text": "hi"}))
            .await
            .expect("call");
        assert_eq!(out, "echo: hi");
    }
}
