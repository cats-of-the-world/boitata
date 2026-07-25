// Git operations. Each tool is a thin, structured wrapper over `git`.
//
// Non-zero exits (e.g. "nothing to commit") are returned as output rather than
// errors, so the agent sees git's message and can react.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::exec;
use crate::tools::workspace;
use crate::tools::{Result, Tool, ToolAnnotations, ToolError, ToolOutput};

/// Shows the working-tree status.
pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show the git working-tree status (short format, with branch info)."
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::read_only()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cwd": {"type": "string", "description": "Repository directory (defaults to the current directory)"}
            }
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        exec::run(
            "git",
            vec![
                "status".to_string(),
                "--short".to_string(),
                "--branch".to_string(),
            ],
            cwd.as_deref(),
            exec::DEFAULT_TIMEOUT,
        )
        .await
    }
}

/// Shows changes (unstaged by default, or staged).
pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show git changes as a diff. By default shows unstaged changes; set \
         `staged` to show staged changes (the two are separate views, not \
         combined). Untracked files are not shown; use `git_status` to see \
         those. Optionally limit to a path."
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::read_only()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": {"type": "boolean", "description": "Show staged changes instead of unstaged (default false)"},
                "path": {"type": "string", "description": "Limit the diff to this file or directory"},
                "cwd": {"type": "string", "description": "Repository directory (defaults to the current directory)"}
            }
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        let mut args = vec!["diff".to_string()];
        if exec::opt_bool_arg(&arguments, "staged") {
            args.push("--staged".to_string());
        }
        if let Some(path) = exec::opt_str_arg(&arguments, "path") {
            // Confine the path to the workspace root (no-op unless one is set),
            // consistent with the fs and search tools.
            let confined = workspace::confine(&path)?;
            args.push("--".to_string());
            args.push(confined.to_string_lossy().into_owned());
        }
        exec::run("git", args, cwd.as_deref(), exec::DEFAULT_TIMEOUT).await
    }
}

/// Creates a commit.
pub struct GitCommitTool;

#[async_trait]
impl Tool for GitCommitTool {
    fn name(&self) -> &str {
        "git_commit"
    }

    fn description(&self) -> &str {
        "Create a git commit with the given message. Set `all` to stage all \
         tracked modified files first (equivalent to `git commit -a`). Does not push."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {"type": "string", "description": "The commit message"},
                "all": {"type": "boolean", "description": "Stage all tracked modified files before committing (default false)"},
                "cwd": {"type": "string", "description": "Repository directory (defaults to the current directory)"}
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let message = exec::str_arg(&arguments, "message", self.name())?;
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        let mut args = vec!["commit".to_string()];
        if exec::opt_bool_arg(&arguments, "all") {
            args.push("--all".to_string());
        }
        // Attached form so a message starting with `-` can't be read as a flag.
        args.push(format!("--message={message}"));
        exec::run("git", args, cwd.as_deref(), exec::DEFAULT_TIMEOUT).await
    }
}

/// Lists, creates, or switches branches.
pub struct GitBranchTool;

impl GitBranchTool {
    /// Read and validate the `name` argument. Rejects names starting with `-`,
    /// which git would otherwise parse as an option (and which are invalid
    /// branch names anyway).
    fn branch_name(&self, arguments: &Value) -> Result<String> {
        let name = exec::str_arg(arguments, "name", self.name())?;
        if name.starts_with('-') {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: format!("branch name `{name}` must not start with '-'"),
            });
        }
        Ok(name.to_string())
    }
}

#[async_trait]
impl Tool for GitBranchTool {
    fn name(&self) -> &str {
        "git_branch"
    }

    fn description(&self) -> &str {
        "Manage git branches. `action` is one of: `list` (default), `create` \
         (create and switch to `name`), or `switch` (switch to `name`)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "create", "switch"], "description": "The branch operation (default 'list')"},
                "name": {"type": "string", "description": "Branch name (required for create/switch)"},
                "cwd": {"type": "string", "description": "Repository directory (defaults to the current directory)"}
            }
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        let action = exec::opt_str_arg(&arguments, "action").unwrap_or_else(|| "list".to_string());

        let args = match action.as_str() {
            "list" => vec!["branch".to_string(), "--list".to_string()],
            "create" => {
                let name = self.branch_name(&arguments)?;
                // No `--` here (unlike `switch`): `name` is the required argument
                // to `-b`, so git takes it literally rather than as a flag, and
                // `branch_name` has already rejected any leading `-`. Inserting
                // `--` would make git read it *as* the branch name and fail.
                vec!["checkout".to_string(), "-b".to_string(), name]
            }
            "switch" => {
                let name = self.branch_name(&arguments)?;
                vec!["checkout".to_string(), "--".to_string(), name]
            }
            other => {
                return Err(ToolError::InvalidArguments {
                    name: self.name().to_string(),
                    reason: format!("unknown action `{other}` (expected list, create, or switch)"),
                });
            }
        };
        exec::run("git", args, cwd.as_deref(), exec::DEFAULT_TIMEOUT).await
    }
}
