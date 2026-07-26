// Code search via ripgrep.

use async_trait::async_trait;
use serde_json::{Value, json};

use tokio_util::sync::CancellationToken;

use super::exec;
use crate::tools::workspace;
use crate::tools::{Result, Tool, ToolAnnotations, ToolError, ToolOutput};

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

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::read_only()
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

    async fn execute(&self, arguments: Value, cancel: CancellationToken) -> Result<ToolOutput> {
        let pattern = exec::str_arg(&arguments, "pattern", self.name())?;
        if pattern.trim().is_empty() {
            // An empty/whitespace pattern is an empty regex that matches every
            // line of every file; scan the whole tree for nothing useful.
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "`pattern` must not be empty".to_string(),
            });
        }
        // Confine the search root to the workspace (no-op unless one is set).
        let raw_path = exec::opt_str_arg(&arguments, "path").unwrap_or_else(|| ".".to_string());
        let path = workspace::confine(&raw_path)?
            .to_string_lossy()
            .into_owned();

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
            // Attached form so a glob starting with `-` (e.g. "-i") can't be read
            // as a ripgrep flag.
            args.push(format!("--glob={glob}"));
        }
        // End flag parsing so a pattern/path starting with `-` (e.g. "--json")
        // is treated as a positional argument, not a ripgrep flag.
        args.push("--".to_string());
        args.push(pattern.to_string());
        args.push(path);

        let output = exec::run_raw("rg", args, None, exec::DEFAULT_TIMEOUT, &cancel).await?;

        // ripgrep exit codes: 0 = matches, 1 = no matches, 2 = error.
        match output.code {
            Some(0) => {
                // Exit 0 means matches, but guard against empty/whitespace-only
                // output so the agent never gets a bare "" with no explanation.
                let trimmed = output.stdout.trim_end();
                if trimmed.is_empty() {
                    Ok(ToolOutput::text("(no matches found)"))
                } else {
                    Ok(ToolOutput::text(trimmed))
                }
            }
            Some(1) => Ok(ToolOutput::text("(no matches found)")),
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
        // The search tool shells out to ripgrep; skip if it isn't installed.
        if std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping test_search_hit_and_miss: ripgrep (rg) not installed");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha needle beta\n").unwrap();
        let path = dir.path().to_string_lossy().into_owned();

        let hit = SearchTool
            .execute(
                serde_json::json!({"pattern": "needle", "path": path.clone()}),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .to_text();
        assert!(hit.contains("needle"), "{hit}");

        let miss = SearchTool
            .execute(
                serde_json::json!({"pattern": "zzz_absent", "path": path}),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .to_text();
        assert_eq!(miss, "(no matches found)");
    }
}
