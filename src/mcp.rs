// MCP (Model Context Protocol) integration.
//
// Connects to MCP servers using the official `rmcp` client, discovers their
// tools, and adapts each one into a [`crate::tools::Tool`] so the agent can call
// it exactly like a built-in. Because MCP tools flow through the normal agent
// loop, their calls are captured by the audit log automatically.
//
// Two transports are supported, inferred from the server config: `command` for
// stdio (spawned subprocess) and `url` for Streamable HTTP (remote server).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ReadResourceRequestParams, ReadResourceResult,
        ResourceContents,
    },
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
use tokio::time::timeout;

use crate::config::{McpServerConfig, McpTransport};
use crate::provider::{ToolContent, tool_content_text};
use crate::tools::{Tool, ToolAnnotations, ToolError, ToolOutput};
use tokio_util::sync::CancellationToken;

/// Maximum length of a tool name exposed to the model (provider schemas cap
/// names at 64 chars and restrict the character set).
const MAX_TOOL_NAME_LEN: usize = 64;

/// Deadline for the initialize handshake — a server that accepts the connection
/// but then hangs must not stall the whole run.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for listing a server's tools.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for a single tool call.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

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
        let name = &config.name;
        let service = match config.transport()? {
            McpTransport::Http {
                url,
                auth_token,
                headers,
            } => {
                // Build via `from_config` without naming the concrete HTTP client
                // type: rmcp bundles its own reqwest version, distinct from ours.
                let transport = StreamableHttpClientTransport::from_config(build_http_config(
                    url, auth_token, headers,
                )?);
                timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
                    .await
                    .map_err(|_| anyhow!("MCP handshake timed out for `{name}` ({url})"))?
                    .with_context(|| format!("MCP handshake failed for `{name}` ({url})"))?
            }
            McpTransport::Stdio { command, args, env } => {
                let transport = build_stdio_transport(name, command, args, env)?;
                timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
                    .await
                    .map_err(|_| anyhow!("MCP handshake timed out for `{name}`"))?
                    .with_context(|| format!("MCP handshake failed for `{name}`"))?
            }
        };

        Ok(Arc::new(Self {
            name: config.name.clone(),
            service,
        }))
    }

    /// Discover the server's tools and wrap each as a registrable [`Tool`].
    pub async fn discover_tools(self: &Arc<Self>) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
        let tools = timeout(DISCOVERY_TIMEOUT, self.service.list_all_tools())
            .await
            .map_err(|_| anyhow!("tool discovery timed out for `{}`", self.name))?
            .with_context(|| format!("tool discovery failed for `{}`", self.name))?;

        let mut out: Vec<Arc<dyn Tool>> = Vec::with_capacity(tools.len());
        // Names are namespaced with the server name and sanitized/truncated, so
        // two distinct remote tools can collide. Detect that instead of letting
        // `ToolRegistry::register` silently overwrite one of them.
        let mut seen = HashSet::with_capacity(tools.len());
        for tool in tools {
            let remote_name = tool.name.to_string();
            let exposed_name = sanitize_tool_name(&format!("{}_{}", self.name, remote_name));

            if !seen.insert(exposed_name.clone()) {
                tracing::warn!(
                    "MCP server `{}`: tool `{}` maps to duplicate name `{}` after sanitization; skipping",
                    self.name,
                    remote_name,
                    exposed_name
                );
                continue;
            }

            let description = tool.description.map(|d| d.to_string()).unwrap_or_default();
            let input_schema = Value::Object((*tool.input_schema).clone());
            // Project the server's MCP annotations onto ours, applying the MCP
            // spec default *per hint* whether or not an annotations object was
            // sent. This makes an absent object and an empty one behave
            // identically (both open-world, per the spec) rather than an absent
            // object falling back to our closed-world `Default`.
            let ann = tool.annotations.as_ref();
            let annotations = ToolAnnotations {
                read_only: ann.and_then(|a| a.read_only_hint).unwrap_or(false),
                destructive: ann.and_then(|a| a.destructive_hint).unwrap_or(true),
                idempotent: ann.and_then(|a| a.idempotent_hint).unwrap_or(false),
                open_world: ann.and_then(|a| a.open_world_hint).unwrap_or(true),
            };

            out.push(Arc::new(McpTool {
                client: Arc::clone(self),
                remote_name,
                exposed_name,
                description,
                input_schema,
                annotations,
            }));
        }

        // Expose resource access (context gathering) as tools when the server
        // supports it, so the agent can list and read resources on demand.
        if self.supports_resources() {
            out.extend(self.resource_tools(&mut seen));
        }

        Ok(out)
    }

    /// Invoke a tool by its server-side name and return its content (text and/or
    /// images).
    async fn call(&self, remote_name: &str, arguments: Value) -> anyhow::Result<Vec<ToolContent>> {
        let mut params = CallToolRequestParams::new(remote_name.to_string());
        if let Some(object) = arguments.as_object() {
            params = params.with_arguments(object.clone());
        }

        let result = timeout(CALL_TIMEOUT, self.service.call_tool(params))
            .await
            .map_err(|_| anyhow!("MCP tool `{remote_name}` timed out"))?
            .map_err(|e| anyhow!("MCP tool `{remote_name}` call failed: {e}"))?;

        let content = result_content(&result);
        if result.is_error.unwrap_or(false) {
            // Flatten to text for the error message.
            let text = tool_content_text(&content);
            return Err(anyhow!(if text.is_empty() {
                format!("MCP tool `{remote_name}` reported an error")
            } else {
                text
            }));
        }
        Ok(content)
    }

    /// Whether the server advertised the `resources` capability at initialize.
    fn supports_resources(&self) -> bool {
        self.service
            .peer_info()
            .map(|info| info.capabilities.resources.is_some())
            .unwrap_or(false)
    }

    /// List the server's resources as a human-readable summary (one per line).
    async fn list_resources(&self) -> anyhow::Result<String> {
        let resources = timeout(DISCOVERY_TIMEOUT, self.service.list_all_resources())
            .await
            .map_err(|_| anyhow!("resource listing timed out for `{}`", self.name))?
            .with_context(|| format!("resource listing failed for `{}`", self.name))?;

        if resources.is_empty() {
            return Ok("(this server exposes no resources)".to_string());
        }

        let mut out = String::new();
        for resource in resources {
            out.push_str(&format!("- {} ({})", resource.uri, resource.name));
            if let Some(mime) = &resource.mime_type {
                out.push_str(&format!(" [{mime}]"));
            }
            if let Some(description) = &resource.description {
                out.push_str(&format!(": {description}"));
            }
            out.push('\n');
        }
        Ok(out.trim_end().to_string())
    }

    /// Read a resource by URI and flatten its contents to text.
    async fn read_resource(&self, uri: &str) -> anyhow::Result<String> {
        let params = ReadResourceRequestParams::new(uri.to_string());
        let result = timeout(CALL_TIMEOUT, self.service.read_resource(params))
            .await
            .map_err(|_| anyhow!("reading resource `{uri}` timed out"))?
            .map_err(|e| anyhow!("reading resource `{uri}` failed: {e}"))?;
        Ok(resource_contents_text(&result))
    }

    /// Build the `list_resources`/`read_resource` tools for this server, skipping
    /// any whose name collides with an already-registered tool.
    fn resource_tools(self: &Arc<Self>, seen: &mut HashSet<String>) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

        let list_name = sanitize_tool_name(&format!("{}_list_resources", self.name));
        if seen.insert(list_name.clone()) {
            tools.push(Arc::new(McpListResourcesTool {
                client: Arc::clone(self),
                name: list_name,
            }));
        } else {
            tracing::warn!(
                "MCP server `{}`: resource list tool collides with existing name `{}`; skipping",
                self.name,
                list_name
            );
        }

        let read_name = sanitize_tool_name(&format!("{}_read_resource", self.name));
        if seen.insert(read_name.clone()) {
            tools.push(Arc::new(McpReadResourceTool {
                client: Arc::clone(self),
                name: read_name,
            }));
        } else {
            tracing::warn!(
                "MCP server `{}`: resource read tool collides with existing name `{}`; skipping",
                self.name,
                read_name
            );
        }

        tools
    }
}

/// Build a stdio transport that spawns the server as a subprocess.
///
/// `kill_on_drop` ensures the OS reaps the child when the handle is dropped,
/// even if the graceful MCP shutdown is ignored or the process is wedged.
fn build_stdio_transport(
    name: &str,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> anyhow::Result<TokioChildProcess> {
    let child = Command::new(command).configure(|cmd| {
        cmd.args(args);
        cmd.envs(env);
        cmd.kill_on_drop(true);
    });
    TokioChildProcess::new(child)
        .with_context(|| format!("failed to spawn MCP server `{name}` (command: {command})"))
}

/// Build the Streamable HTTP transport config for a remote server, applying the
/// optional bearer token and any custom headers. Returns the config (not the
/// transport) so the reqwest-typed transport is only ever constructed via
/// `from_config`, never named — rmcp bundles a different reqwest version.
fn build_http_config(
    url: &str,
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
) -> anyhow::Result<StreamableHttpClientTransportConfig> {
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.to_string());

    if let Some(token) = auth_token {
        // rmcp adds the `Bearer ` prefix itself.
        transport_config = transport_config.auth_header(token.to_string());
    }

    if !headers.is_empty() {
        let mut header_map = HashMap::with_capacity(headers.len());
        for (key, value) in headers {
            let name = HeaderName::from_bytes(key.as_bytes())
                .with_context(|| format!("invalid HTTP header name `{key}`"))?;
            let value = HeaderValue::from_str(value)
                .with_context(|| format!("invalid HTTP header value for `{key}`"))?;
            header_map.insert(name, value);
        }
        transport_config = transport_config.custom_headers(header_map);
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
    /// Side-effect hints projected from the server's MCP `ToolAnnotations`.
    annotations: ToolAnnotations,
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

    fn annotations(&self) -> ToolAnnotations {
        self.annotations
    }

    async fn execute(
        &self,
        arguments: Value,
        cancel: CancellationToken,
    ) -> crate::tools::Result<ToolOutput> {
        // Race the remote call against cancellation; on cancel the call future is
        // dropped, aborting the in-flight request.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError::Cancelled(format!(
                "MCP tool `{}`",
                self.remote_name
            ))),
            result = self.client.call(&self.remote_name, arguments) => result
                .map(|content| ToolOutput { content })
                .map_err(|e| ToolError::ExecutionFailed(e.to_string())),
        }
    }
}

/// Lists the resources exposed by one MCP server. Adapts the server's
/// `resources/list` to a [`Tool`] the agent can call for context gathering.
struct McpListResourcesTool {
    client: Arc<McpClient>,
    name: String,
}

#[async_trait]
impl Tool for McpListResourcesTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "List the resources (documents and data) this MCP server exposes, with \
         their URIs, names, and descriptions. Use a returned URI with the matching \
         read_resource tool to fetch its contents."
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::read_only()
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {}, "required": []})
    }

    async fn execute(
        &self,
        _arguments: Value,
        cancel: CancellationToken,
    ) -> crate::tools::Result<ToolOutput> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError::Cancelled("list_resources".to_string())),
            result = self.client.list_resources() => result
                .map(ToolOutput::from)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string())),
        }
    }
}

/// Reads a single resource from one MCP server by URI. Adapts the server's
/// `resources/read` to a [`Tool`].
struct McpReadResourceTool {
    client: Arc<McpClient>,
    name: String,
}

#[async_trait]
impl Tool for McpReadResourceTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Read the contents of a resource from this MCP server by its URI. Get \
         available URIs from the matching list_resources tool."
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::read_only()
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string", "description": "The URI of the resource to read"}
            },
            "required": ["uri"]
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        cancel: CancellationToken,
    ) -> crate::tools::Result<ToolOutput> {
        let uri = arguments
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments {
                name: self.name.clone(),
                reason: "missing 'uri' argument".to_string(),
            })?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError::Cancelled("read_resource".to_string())),
            result = self.client.read_resource(uri) => result
                .map(ToolOutput::from)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string())),
        }
    }
}

/// Flatten a resource read result to text. Text contents are concatenated; blob
/// contents are noted (their base64 payload is omitted to keep the context lean).
fn resource_contents_text(result: &ReadResourceResult) -> String {
    let mut parts = Vec::new();
    for content in &result.contents {
        match content {
            ResourceContents::TextResourceContents { text, .. } => parts.push(text.clone()),
            ResourceContents::BlobResourceContents {
                mime_type, blob, ..
            } => {
                let kind = mime_type.as_deref().unwrap_or("binary");
                // `blob` is the base64-encoded payload, so this is its encoded
                // character count, not the decoded byte size.
                parts.push(format!(
                    "[{kind} resource — {} base64 characters omitted]",
                    blob.len()
                ));
            }
            // `ResourceContents` is #[non_exhaustive]; ignore future variants.
            _ => parts.push("[unsupported resource content omitted]".to_string()),
        }
    }
    parts.join("\n")
}

/// Convert an MCP tool result into Boitata tool content. Text and image blocks
/// are preserved; other block types are noted; an empty result falls back to any
/// structured content (as text).
fn result_content(result: &CallToolResult) -> Vec<ToolContent> {
    let mut parts = Vec::new();
    for block in &result.content {
        if let Some(text) = block.as_text() {
            parts.push(ToolContent::text(text.text.clone()));
        } else if let Some(image) = block.as_image() {
            parts.push(ToolContent::Image {
                mime_type: image.mime_type.clone(),
                data: image.data.clone(),
            });
        } else {
            parts.push(ToolContent::text("[non-text content omitted]"));
        }
    }
    if parts.is_empty() {
        if let Some(structured) = &result.structured_content {
            return vec![ToolContent::text(structured.to_string())];
        }
    }
    parts
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
    fn test_result_content_joins_text_blocks() {
        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"}
            ]
        }))
        .unwrap();
        assert_eq!(tool_content_text(&result_content(&result)), "hello\nworld");
    }

    #[test]
    fn test_result_content_preserves_images() {
        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [
                {"type": "text", "text": "see"},
                {"type": "image", "data": "AAAA", "mimeType": "image/png"}
            ]
        }))
        .unwrap();
        let content = result_content(&result);
        assert_eq!(content.len(), 2);
        assert!(matches!(&content[0], ToolContent::Text { text } if text == "see"));
        assert!(matches!(
            &content[1],
            ToolContent::Image { mime_type, data } if mime_type == "image/png" && data == "AAAA"
        ));
    }

    #[test]
    fn test_result_content_falls_back_to_structured() {
        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [],
            "structuredContent": {"ok": true}
        }))
        .unwrap();
        assert_eq!(
            tool_content_text(&result_content(&result)),
            r#"{"ok":true}"#
        );
    }

    // Transport inference/validation is tested in `config` (McpServerConfig::transport).

    #[test]
    fn test_http_config_applies_auth_token() {
        // `auth_header` is a documented public field of rmcp's transport config.
        let cfg =
            build_http_config("http://localhost/mcp", Some("secret"), &HashMap::new()).unwrap();
        assert_eq!(cfg.auth_header.as_deref(), Some("secret"));
    }

    #[test]
    fn test_http_config_rejects_invalid_header() {
        let headers = HashMap::from([("bad header".to_string(), "v".to_string())]);
        assert!(build_http_config("http://localhost/mcp", None, &headers).is_err());
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
                        "capabilities": {"tools": {}, "resources": {}},
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
                "resources/list" => Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"resources": [{
                        "uri": "mem://note",
                        "name": "note",
                        "description": "A test note",
                        "mimeType": "text/plain",
                    }]}
                })),
                "resources/read" => {
                    let uri = req["params"]["uri"].as_str().unwrap_or("").to_string();
                    Some(serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"contents": [{
                            "uri": uri,
                            "mimeType": "text/plain",
                            "text": "resource body",
                        }]}
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

        // The regular tool plus the two resource tools (server advertised the
        // resources capability), all namespaced with the server name.
        let by_name: std::collections::HashMap<&str, &Arc<dyn Tool>> =
            tools.iter().map(|t| (t.name(), t)).collect();
        assert!(by_name.contains_key("test_echo"), "{:?}", by_name.keys());
        // The test server sends no annotations object for `echo`, so the MCP spec
        // defaults apply per hint: not read-only, and open-world (external).
        let echo_ann = by_name["test_echo"].annotations();
        assert!(!echo_ann.read_only);
        assert!(echo_ann.open_world);
        assert!(
            by_name.contains_key("test_list_resources"),
            "{:?}",
            by_name.keys()
        );
        assert!(
            by_name.contains_key("test_read_resource"),
            "{:?}",
            by_name.keys()
        );

        // Calling the tool round-trips through rmcp and returns the server output.
        let echo = by_name["test_echo"]
            .execute(serde_json::json!({"text": "hi"}), CancellationToken::new())
            .await
            .expect("call echo")
            .to_text();
        assert_eq!(echo, "echo: hi");

        // A pre-cancelled token short-circuits the call (the `biased` select
        // picks the ready `cancelled()` branch first) and returns `Cancelled`.
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let err = by_name["test_echo"]
            .execute(serde_json::json!({"text": "hi"}), cancelled)
            .await
            .expect_err("cancelled call should error");
        assert!(matches!(err, ToolError::Cancelled(_)), "{err:?}");

        // Listing resources returns the server's resource summary.
        let listed = by_name["test_list_resources"]
            .execute(serde_json::json!({}), CancellationToken::new())
            .await
            .expect("list resources")
            .to_text();
        assert!(listed.contains("mem://note"), "{listed}");

        // Reading a resource returns its contents.
        let body = by_name["test_read_resource"]
            .execute(
                serde_json::json!({"uri": "mem://note"}),
                CancellationToken::new(),
            )
            .await
            .expect("read resource")
            .to_text();
        assert_eq!(body, "resource body");
    }
}
