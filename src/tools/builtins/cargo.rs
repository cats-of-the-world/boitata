// Cargo operations — deterministic wrappers over the Rust toolchain.
//
// Compiler/linter/test failures come back as normal output (with the exit code)
// so the agent can read them and iterate.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::exec;
use crate::tools::{Result, Tool, ToolError, ToolOutput};

/// Optional working-directory property shared by the cargo tools.
fn cwd_property() -> Value {
    json!({"type": "string", "description": "Crate/workspace directory (defaults to the current directory)"})
}

/// `cargo check` — type-check without producing binaries.
pub struct CargoCheckTool;

#[async_trait]
impl Tool for CargoCheckTool {
    fn name(&self) -> &str {
        "cargo_check"
    }

    fn description(&self) -> &str {
        "Run `cargo check` to type-check the crate. Reports compiler errors and warnings."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"cwd": cwd_property()}})
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        exec::run(
            "cargo",
            vec!["check".to_string()],
            cwd.as_deref(),
            exec::BUILD_TIMEOUT,
        )
        .await
    }
}

/// `cargo clippy` — lint, optionally applying automatic fixes.
pub struct CargoClippyTool;

#[async_trait]
impl Tool for CargoClippyTool {
    fn name(&self) -> &str {
        "cargo_clippy"
    }

    fn description(&self) -> &str {
        "Run `cargo clippy` to lint the crate. Set `fix` to apply Clippy's \
         automatic fixes in place (uses --allow-dirty --allow-staged)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fix": {"type": "boolean", "description": "Apply automatic fixes in place (default false)"},
                "cwd": cwd_property()
            }
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        let mut args = vec!["clippy".to_string()];
        if exec::opt_bool_arg(&arguments, "fix") {
            args.push("--fix".to_string());
            args.push("--allow-dirty".to_string());
            args.push("--allow-staged".to_string());
        }
        exec::run("cargo", args, cwd.as_deref(), exec::BUILD_TIMEOUT).await
    }
}

/// `cargo fmt` — format code, or check formatting without writing.
pub struct CargoFmtTool;

#[async_trait]
impl Tool for CargoFmtTool {
    fn name(&self) -> &str {
        "cargo_fmt"
    }

    fn description(&self) -> &str {
        "Run `cargo fmt` to format the crate. By default it reformats files in \
         place; set `check` to only report what would change without writing."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "check": {"type": "boolean", "description": "Only check formatting, don't modify files (default false)"},
                "cwd": cwd_property()
            }
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        let mut args = vec!["fmt".to_string()];
        if exec::opt_bool_arg(&arguments, "check") {
            args.push("--check".to_string());
        }
        exec::run("cargo", args, cwd.as_deref(), exec::DEFAULT_TIMEOUT).await
    }
}

/// `cargo test` — run the test suite, optionally filtered.
pub struct CargoTestTool;

#[async_trait]
impl Tool for CargoTestTool {
    fn name(&self) -> &str {
        "cargo_test"
    }

    fn description(&self) -> &str {
        "Run `cargo test`. Optionally pass `filter` to run only tests whose name \
         contains that substring."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filter": {"type": "string", "description": "Only run tests matching this name substring"},
                "cwd": cwd_property()
            }
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        let mut args = vec!["test".to_string()];
        if let Some(filter) = exec::opt_str_arg(&arguments, "filter") {
            // `--` ends option parsing so a filter like "--help" is treated as a
            // test-name filter rather than a flag.
            args.push("--".to_string());
            args.push(filter);
        }
        exec::run("cargo", args, cwd.as_deref(), exec::BUILD_TIMEOUT).await
    }
}

/// `cargo add` — add a dependency.
pub struct CargoAddTool;

#[async_trait]
impl Tool for CargoAddTool {
    fn name(&self) -> &str {
        "cargo_add"
    }

    fn description(&self) -> &str {
        "Add a dependency to Cargo.toml with `cargo add`. Optionally enable \
         `features` and add it as a dev-dependency."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "crate": {"type": "string", "description": "The crate to add, optionally with a version (e.g. 'serde@1')"},
                "features": {"type": "array", "items": {"type": "string"}, "description": "Features to enable"},
                "dev": {"type": "boolean", "description": "Add as a dev-dependency (default false)"},
                "cwd": cwd_property()
            },
            "required": ["crate"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let krate = exec::str_arg(&arguments, "crate", self.name())?;
        // Reject option-like names so a value such as "--config" can't be read
        // by `cargo add` as a flag instead of a crate spec.
        if krate.starts_with('-') {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: format!("crate `{krate}` must not start with '-'"),
            });
        }
        let cwd = exec::opt_str_arg(&arguments, "cwd");
        let mut args = vec!["add".to_string()];
        if exec::opt_bool_arg(&arguments, "dev") {
            args.push("--dev".to_string());
        }
        if let Some(features) = arguments.get("features").and_then(|v| v.as_array()) {
            let features: Vec<&str> = features.iter().filter_map(|v| v.as_str()).collect();
            // Reject option-like feature names for consistency with the crate
            // check above; a `-`-leading value could surprise cargo's parsing.
            for f in &features {
                if f.starts_with('-') {
                    return Err(ToolError::InvalidArguments {
                        name: self.name().to_string(),
                        reason: format!("feature `{f}` must not start with '-'"),
                    });
                }
            }
            if !features.is_empty() {
                args.push("--features".to_string());
                args.push(features.join(","));
            }
        }
        // `--` must come after the named flags (--dev/--features); it ends option
        // parsing so the crate spec that follows is never read as a flag.
        args.push("--".to_string());
        args.push(krate.to_string());
        exec::run("cargo", args, cwd.as_deref(), exec::BUILD_TIMEOUT).await
    }
}
