// Tools module: Tool registry and built-in tool implementations

pub mod builtins;
pub mod workspace;

use crate::provider::{ToolContent, ToolDefinition, tool_content_text};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

// Re-export built-in tools
pub use builtins::{
    CargoAddTool, CargoCheckTool, CargoClippyTool, CargoFmtTool, CargoTestTool, ExecuteCommandTool,
    FileReadTool, FileWriteTool, GitBranchTool, GitCommitTool, GitDiffTool, GitStatusTool,
    ListDirectoryTool, SearchTool,
};

/// Result type for tool operations
pub type Result<T> = std::result::Result<T, ToolError>;

/// Errors that can occur during tool operations
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Tool not found: {0}")]
    NotFound(String),

    #[error("Invalid arguments for tool {name}: {reason}")]
    InvalidArguments { name: String, reason: String },

    #[error("Tool execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Other error: {0}")]
    Other(String),
}

/// Structured output from a tool: an ordered list of content parts (text and/or
/// images), mirroring goose/MCP tool results. Most tools produce a single text
/// part; use [`ToolOutput::text`] or the `From<String>`/`From<&str>`
/// conversions for that common case.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub content: Vec<ToolContent>,
}

impl ToolOutput {
    /// A single text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::text(text)],
        }
    }

    /// Flatten to a single string for text-only sinks (the CLI summary, the
    /// audit log, and providers whose tool-result role accepts only text).
    /// Images collapse to a short placeholder (see [`tool_content_text`]).
    pub fn to_text(&self) -> String {
        tool_content_text(&self.content)
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        ToolOutput::text(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        ToolOutput::text(text)
    }
}

/// Hints describing a tool's side effects, mirroring MCP `ToolAnnotations`.
/// Used to drive local policy (e.g. auto-approving read-only tools) and for
/// observability in the audit log.
///
/// The [`Default`] is deliberately conservative: assume a tool may make
/// destructive changes to a closed environment. Read-only tools opt in via
/// [`ToolAnnotations::read_only`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolAnnotations {
    /// The tool does not modify its environment.
    pub read_only: bool,
    /// The tool may perform destructive/irreversible updates. Only meaningful
    /// when `read_only` is false.
    pub destructive: bool,
    /// Repeated calls with the same arguments have no additional effect.
    pub idempotent: bool,
    /// The tool interacts with an open world (network, external systems).
    pub open_world: bool,
}

impl Default for ToolAnnotations {
    fn default() -> Self {
        Self {
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: false,
        }
    }
}

impl ToolAnnotations {
    /// A pure reader: no modifications, safe to repeat, closed world.
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        }
    }
}

/// Trait for tools that can be executed by the agent
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name
    fn name(&self) -> &str;

    /// Get the tool description
    fn description(&self) -> &str;

    /// Get the JSON schema for the tool's input arguments
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given arguments.
    ///
    /// `cancel` is triggered when the run is interrupted (e.g. Ctrl-C).
    /// Long-running tools (subprocesses, remote calls) should stop promptly when
    /// it fires; quick, bounded tools may ignore it.
    async fn execute(
        &self,
        arguments: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutput>;

    /// Side-effect hints for this tool. Defaults to the conservative
    /// [`ToolAnnotations::default`] (assume the tool may modify state); pure
    /// readers should override this with [`ToolAnnotations::read_only`].
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
    }
}

/// Registry for tools
#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool, returning `true` if it was added.
    ///
    /// Names are unique keys the model uses to invoke a tool, so a collision
    /// would otherwise silently overwrite one implementation with another. When
    /// a tool with the same name is already registered, the existing one is
    /// **kept**, the duplicate is dropped, and this warns and returns `false`.
    ///
    /// Built-in tools have statically distinct names; this guards the dynamic
    /// paths, where an MCP server's namespaced tool name can collide with a
    /// built-in or with another server's tool (`mcp.rs` already de-duplicates
    /// within a single server, but not across servers). Registering built-ins
    /// before MCP tools therefore means a built-in always wins the name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> bool {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            tracing::warn!(
                "tool `{name}` is already registered; keeping the existing one and ignoring the duplicate"
            );
            return false;
        }
        self.tools.insert(name, tool);
        true
    }

    /// Execute a tool by name. `cancel` is forwarded to the tool so an
    /// interrupted run can stop a long-running tool promptly.
    pub async fn execute(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?;

        tool.execute(arguments.clone(), cancel).await
    }

    /// Look up a registered tool's side-effect annotations, if it exists.
    pub fn annotations(&self, name: &str) -> Option<ToolAnnotations> {
        self.tools.get(name).map(|tool| tool.annotations())
    }

    /// Check if a tool exists
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Get all tool names
    pub fn list_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Convert all tools to provider tool definitions
    pub fn to_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool {
        name: String,
    }

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "A dummy tool for testing"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            })
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput::text("success"))
        }
    }

    #[tokio::test]
    async fn test_registry() {
        let mut registry = ToolRegistry::new();
        let tool = Arc::new(DummyTool {
            name: "dummy".to_string(),
        });

        assert!(registry.register(tool.clone()));

        assert!(registry.has_tool("dummy"));
        assert_eq!(registry.list_names(), vec!["dummy".to_string()]);

        let result = registry
            .execute("dummy", &serde_json::json!({}), CancellationToken::new())
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().to_text(), "success");
    }

    #[test]
    fn test_register_rejects_duplicate_names() {
        let mut registry = ToolRegistry::new();
        let first = Arc::new(DummyTool {
            name: "dup".to_string(),
        });
        let second = Arc::new(DummyTool {
            name: "dup".to_string(),
        });

        assert!(registry.register(first.clone()));
        // Same name: the duplicate is refused and the first registration is kept.
        assert!(!registry.register(second));
        assert_eq!(registry.list_names(), vec!["dup".to_string()]);
        // The kept tool is the original instance.
        assert!(Arc::ptr_eq(
            &registry.tools["dup"],
            &(first as Arc<dyn Tool>)
        ));
    }

    #[test]
    fn test_to_definitions() {
        let mut registry = ToolRegistry::new();
        let tool = Arc::new(DummyTool {
            name: "dummy".to_string(),
        });

        registry.register(tool);

        let defs = registry.to_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "dummy");
        assert_eq!(defs[0].description, "A dummy tool for testing");
    }
}
