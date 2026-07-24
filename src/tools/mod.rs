// Tools module: Tool registry and built-in tool implementations

pub mod builtins;

use crate::provider::ToolDefinition;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

// Re-export built-in tools
pub use builtins::fs::{FileReadTool, FileWriteTool, ListDirectoryTool};

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

/// Trait for tools that can be executed by the agent
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name
    fn name(&self) -> &str;

    /// Get the tool description
    fn description(&self) -> &str;

    /// Get the JSON schema for the tool's input arguments
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given arguments
    async fn execute(&self, arguments: serde_json::Value) -> Result<String>;
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

    /// Register a tool
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Execute a tool by name
    pub async fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?;

        tool.execute(arguments.clone()).await
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

        async fn execute(&self, _arguments: serde_json::Value) -> Result<String> {
            Ok("success".to_string())
        }
    }

    #[tokio::test]
    async fn test_registry() {
        let mut registry = ToolRegistry::new();
        let tool = Arc::new(DummyTool {
            name: "dummy".to_string(),
        });

        registry.register(tool.clone());

        assert!(registry.has_tool("dummy"));
        assert_eq!(registry.list_names(), vec!["dummy".to_string()]);

        let result = registry.execute("dummy", &serde_json::json!({})).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
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
