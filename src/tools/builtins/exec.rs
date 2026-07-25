// Shared subprocess execution for the command-oriented built-in tools
// (execute_command, search, git_*, cargo_*).

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use crate::tools::{Result, ToolError};

/// Default wall-clock limit for a single command.
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Longer limit for cargo commands, which may compile from scratch.
pub(super) const BUILD_TIMEOUT: Duration = Duration::from_secs(600);
/// Cap on captured stdout/stderr so a chatty command can't blow up the context.
const MAX_STREAM_BYTES: usize = 30_000;

/// Raw result of running a subprocess (streams already truncated).
pub(super) struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `program` with `args`, capturing output. Only returns `Err` when the
/// process cannot be launched or exceeds `timeout`; a non-zero exit is a normal
/// result so callers can decide how to interpret it.
pub(super) async fn run_raw(
    program: &str,
    args: Vec<String>,
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<Output> {
    let mut command = Command::new(program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let child = command.spawn().map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::NotFound {
            format!(" (is `{program}` installed and on PATH?)")
        } else {
            String::new()
        };
        ToolError::ExecutionFailed(format!("failed to launch `{program}`: {e}{hint}"))
    })?;

    // On timeout the future (owning the child) is dropped; `kill_on_drop` reaps it.
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            ToolError::ExecutionFailed(format!(
                "`{program}` timed out after {}s",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| ToolError::ExecutionFailed(format!("`{program}` failed: {e}")))?;

    Ok(Output {
        code: output.status.code(),
        stdout: truncate(&String::from_utf8_lossy(&output.stdout)),
        stderr: truncate(&String::from_utf8_lossy(&output.stderr)),
    })
}

/// Run a command and format its result as a single string: an exit-status note
/// (only when non-zero), then stdout, then stderr. Non-zero exits are reported
/// in the text — not as errors — so the agent can read compiler/linter/test
/// failures and iterate.
pub(super) async fn run(
    program: &str,
    args: Vec<String>,
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    let output = run_raw(program, args, cwd, timeout).await?;

    let mut sections = Vec::new();
    match output.code {
        Some(0) => {}
        Some(code) => sections.push(format!("[exit code {code}]")),
        None => sections.push("[terminated by signal]".to_string()),
    }
    let stdout = output.stdout.trim_end();
    let stderr = output.stderr.trim_end();
    if !stdout.is_empty() {
        sections.push(stdout.to_string());
    }
    if !stderr.is_empty() {
        sections.push(format!("--- stderr ---\n{stderr}"));
    }
    if sections.is_empty() {
        return Ok("(command completed successfully with no output)".to_string());
    }
    Ok(sections.join("\n"))
}

/// Keep the tail of an oversized stream — build/test errors cluster at the end.
fn truncate(s: &str) -> String {
    if s.len() <= MAX_STREAM_BYTES {
        return s.to_string();
    }
    let mut start = s.len() - MAX_STREAM_BYTES;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("… ({start} earlier bytes truncated)\n{}", &s[start..])
}

// --- argument helpers -------------------------------------------------------

/// Extract a required string argument.
pub(super) fn str_arg<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArguments {
            name: tool.to_string(),
            reason: format!("missing or non-string `{key}` argument"),
        })
}

/// Extract an optional string argument (owned).
pub(super) fn opt_str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract an optional boolean argument, defaulting to `false`.
pub(super) fn opt_bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_run_captures_stdout() {
        let out = run("echo", vec!["hello".to_string()], None, DEFAULT_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn test_run_reports_nonzero_exit() {
        // `sh -c 'exit 3'` completes (non-zero) — reported, not an error.
        let out = run(
            "sh",
            vec!["-c".to_string(), "exit 3".to_string()],
            None,
            DEFAULT_TIMEOUT,
        )
        .await
        .unwrap();
        assert!(out.contains("exit code 3"), "{out}");
    }

    #[tokio::test]
    async fn test_run_missing_program_errors() {
        let err = run("boitata_nope_xyz", vec![], None, DEFAULT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn test_run_times_out() {
        let err = run(
            "sleep",
            vec!["5".to_string()],
            None,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("timed out"), "{err}");
    }

    #[test]
    fn test_arg_helpers() {
        let args = serde_json::json!({"name": "x", "flag": true});
        assert_eq!(str_arg(&args, "name", "t").unwrap(), "x");
        assert!(str_arg(&args, "missing", "t").is_err());
        assert_eq!(opt_str_arg(&args, "missing"), None);
        assert!(opt_bool_arg(&args, "flag"));
        assert!(!opt_bool_arg(&args, "missing"));
    }
}
