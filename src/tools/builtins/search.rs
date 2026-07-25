// Code search via ripgrep.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::exec;
use crate::tools::{Result, Tool, ToolError};

/// Searches file contents with ripgrep (`rg`).
pub struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Search file contents with ripgrep (regex). Returns matching lines with \
         their file paths and line numbers."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "The regex pattern to search for"},
                "path": {"type": "string", "description": "File or directory to search (defaults to the current directory)"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. '*.rs'"},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive search (default false)"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let pattern = exec::str_arg(&arguments, "pattern", self.name())?;
        let path = exec::opt_str_arg(&arguments, "path").unwrap_or_else(|| ".".to_string());

        let mut args = vec![
            "--line-number".to_string(),
            "--no-heading".to_string(),
            "--color".to_string(),
            "never".to_string(),
        ];
        if exec::opt_bool_arg(&arguments, "case_insensitive") {
            args.push("--ignore-case".to_string());
        }
        if let Some(glob) = exec::opt_str_arg(&arguments, "glob") {
            args.push("--glob".to_string());
            args.push(glob);
        }
        args.push(pattern.to_string());
        args.push(path);

        let output = exec::run_raw("rg", args, None, exec::DEFAULT_TIMEOUT).await?;

        // ripgrep exit codes: 0 = matches, 1 = no matches, 2 = error.
        match output.code {
            Some(0) => Ok(output.stdout.trim_end().to_string()),
            Some(1) => Ok("(no matches found)".to_string()),
            _ => {
                let detail = if output.stderr.trim().is_empty() {
                    "unknown error".to_string()
                } else {
                    output.stderr.trim().to_string()
                };
                Err(ToolError::ExecutionFailed(format!(
                    "ripgrep error: {detail}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_search_hit_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha needle beta\n").unwrap();
        let path = dir.path().to_string_lossy().into_owned();

        let hit = SearchTool
            .execute(serde_json::json!({"pattern": "needle", "path": path.clone()}))
            .await
            .unwrap();
        assert!(hit.contains("needle"), "{hit}");

        let miss = SearchTool
            .execute(serde_json::json!({"pattern": "zzz_absent", "path": path}))
            .await
            .unwrap();
        assert_eq!(miss, "(no matches found)");
    }
}
