// Shared subprocess execution for the command-oriented built-in tools
// (execute_command, search, git_*, cargo_*).

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::tools::{Result, ToolError};

/// Default wall-clock limit for a single command.
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Longer limit for cargo commands, which may compile from scratch.
pub(super) const BUILD_TIMEOUT: Duration = Duration::from_secs(600);
/// Approximate cap on captured stdout/stderr so a chatty command can't blow up
/// the context. The kept tail is at most this many bytes; a short "truncated"
/// note may push the final string a handful of bytes over.
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
        // Confine the working directory to the workspace root, consistently with
        // the path-taking tools (no-op unless a root is configured).
        let dir = crate::tools::workspace::confine(dir)?;
        command.current_dir(dir);
    }
    // Put the child in its own process group so a timeout can kill the whole
    // tree — `sh -c` and anything that forks would otherwise leave orphaned
    // grandchildren running. `kill_on_drop` only reaps the direct child.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::NotFound {
            format!(" (is `{program}` installed and on PATH?)")
        } else {
            String::new()
        };
        ToolError::ExecutionFailed(format!("failed to launch `{program}`: {e}{hint}"))
    })?;

    // The child is its own group leader, so its PID is the group ID.
    #[cfg(unix)]
    let pgid = child.id().map(|id| id as i32);

    // Read the pipes with a byte cap (keeping the tail, where build/test errors
    // cluster) *while* waiting, rather than buffering unbounded output in memory
    // via `wait_with_output`. Keeping the child owned here (rather than moving it
    // into the wait future) lets us kill its group on timeout while it's still
    // alive — avoiding a PID-recycling race.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let result = tokio::time::timeout(timeout, async {
        let stdout = read_capped(&mut stdout_pipe, MAX_STREAM_BYTES);
        let stderr = read_capped(&mut stderr_pipe, MAX_STREAM_BYTES);
        let status = child.wait();
        tokio::join!(stdout, stderr, status)
    })
    .await;

    match result {
        Ok((stdout, stderr, status)) => {
            let status = status
                .map_err(|e| ToolError::ExecutionFailed(format!("`{program}` failed: {e}")))?;
            Ok(Output {
                code: status.code(),
                stdout: finalize_stream(stdout),
                stderr: finalize_stream(stderr),
            })
        }
        Err(_elapsed) => {
            // Kill the whole group while the child is still alive (its PID/PGID
            // is still valid), then reap the child.
            #[cfg(unix)]
            if let Some(pgid) = pgid {
                // Safety: signalling a process group is always memory-safe; a
                // group that's already gone just yields ESRCH, which we ignore.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(ToolError::ExecutionFailed(format!(
                "`{program}` timed out after {}s",
                timeout.as_secs()
            )))
        }
    }
}

/// Drain a child pipe, keeping at most `cap` bytes from the *tail* so a chatty
/// command can't exhaust memory. Continues reading past the cap (discarding) so
/// the child never blocks on a full pipe. Returns the kept bytes and whether any
/// were dropped.
async fn read_capped<R: AsyncRead + Unpin>(pipe: &mut Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    let Some(reader) = pipe.as_mut() else {
        return (buf, truncated);
    };
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > cap {
                    let overflow = buf.len() - cap;
                    buf.drain(..overflow);
                    truncated = true;
                }
            }
        }
    }
    (buf, truncated)
}

/// Turn captured bytes into a string, noting up front if earlier output was
/// dropped (we keep the tail).
fn finalize_stream((bytes, truncated): (Vec<u8>, bool)) -> String {
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        format!("… (earlier output truncated to the last {MAX_STREAM_BYTES} bytes)\n{text}")
    } else {
        text
    }
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
