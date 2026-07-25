// Arbitrary command execution.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::exec;
use crate::tools::{Result, Tool, ToolAnnotations, ToolOutput};

/// Runs a shell command. This is the escape hatch for operations without a
/// dedicated tool; it runs with the agent's privileges, so deployments that want
/// to restrict the agent to the structured tools can disable it in config.
pub struct ExecuteCommandTool;

#[async_trait]
impl Tool for ExecuteCommandTool {
    fn name(&self) -> &str {
        "execute_command"
    }

    fn description(&self) -> &str {
        "Run a shell command (via `sh -c`) and return its combined stdout, stderr, \
         and exit code. Use for operations without a dedicated tool; prefer the \
         cargo_*, git_*, and search tools when they apply."
    }

    fn annotations(&self) -> ToolAnnotations {
        // An arbitrary shell command can do anything, including reaching the
        // network, so it is neither read-only nor closed-world.
        ToolAnnotations {
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: true,
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command to run"},
                "cwd": {"type": "string", "description": "Working directory (defaults to the current directory)"}
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let command = exec::str_arg(&arguments, "command", self.name())?;
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        exec::run(
            "sh",
            vec!["-c".to_string(), command.to_string()],
            cwd.as_deref(),
            exec::DEFAULT_TIMEOUT,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_command_runs_shell() {
        let out = ExecuteCommandTool
            .execute(serde_json::json!({"command": "echo boitata"}))
            .await
            .unwrap();
        assert_eq!(out.to_text(), "boitata");
    }
}
