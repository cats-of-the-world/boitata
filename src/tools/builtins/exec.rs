// Shared subprocess execution for the command-oriented built-in tools
// (execute_command, search, git_*, cargo_*).

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::tools::{Result, ToolError, ToolOutput};

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

/// How a subprocess run ended: normally, by timeout, or by external cancellation.
/// The `Finished` payload is the joined `(stdout, stderr, wait-status)` triple.
type JoinedOutput = (
    (Vec<u8>, bool),
    (Vec<u8>, bool),
    std::io::Result<std::process::ExitStatus>,
);
enum RunOutcome {
    Finished(JoinedOutput),
    TimedOut,
    Cancelled,
}

/// Run `program` with `args`, capturing output. Returns `Err` only when the
/// process cannot be launched, exceeds `timeout`, or is cancelled via `cancel`
/// (`ToolError::Cancelled`); a non-zero exit is a normal result so callers can
/// decide how to interpret it.
pub(super) async fn run_raw(
    program: &str,
    args: Vec<String>,
    cwd: Option<&str>,
    timeout: Duration,
    cancel: &CancellationToken,
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

    // Race the command against its timeout *and* an external cancellation (e.g.
    // Ctrl-C). Either interruption kills the child's whole process group so
    // forked grandchildren don't orphan.
    let outcome = tokio::select! {
        joined = tokio::time::timeout(timeout, async {
            let stdout = read_capped(&mut stdout_pipe, MAX_STREAM_BYTES);
            let stderr = read_capped(&mut stderr_pipe, MAX_STREAM_BYTES);
            let status = child.wait();
            tokio::join!(stdout, stderr, status)
        }) => match joined {
            Ok(joined) => RunOutcome::Finished(joined),
            Err(_elapsed) => RunOutcome::TimedOut,
        },
        _ = cancel.cancelled() => RunOutcome::Cancelled,
    };

    let cancelled = matches!(outcome, RunOutcome::Cancelled);
    match outcome {
        RunOutcome::Finished((stdout, stderr, status)) => {
            let status = status
                .map_err(|e| ToolError::ExecutionFailed(format!("`{program}` failed: {e}")))?;
            Ok(Output {
                code: status.code(),
                stdout: finalize_stream(stdout),
                stderr: finalize_stream(stderr),
            })
        }
        RunOutcome::TimedOut | RunOutcome::Cancelled => {
            // Kill the whole group while the child is still alive (its PID/PGID
            // is still valid), then reap the child. Both interruptions share this
            // teardown; only the returned message differs.
            #[cfg(unix)]
            if let Some(pgid) = pgid {
                // Safety: `pgid` is `child.id()`, and `child` is still alive here
                // (owned, not yet reaped), so its PID/PGID is valid and cannot
                // have been recycled by the OS; a later refactor that reaps the
                // child before this kill would reintroduce that race. `libc::kill`
                // is a raw syscall with no invariants to uphold; a group that's
                // already gone just yields ESRCH, which we ignore.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            // A cancel is a user interrupt, not a failure — surface it as such so
            // logs can tell the two apart.
            Err(if cancelled {
                ToolError::Cancelled(format!("`{program}`"))
            } else {
                ToolError::ExecutionFailed(format!(
                    "`{program}` timed out after {}s",
                    timeout.as_secs()
                ))
            })
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
        // A read error on a child pipe is treated as end-of-stream: the capture
        // is best-effort and the child's exit status is the source of truth for
        // success/failure, so we keep whatever we have rather than fail the tool.
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Only trim once the buffer reaches 2x the cap, so the O(cap)
                // memmove is amortized over many chunks instead of running on
                // every read (a 1 MB stream would otherwise memmove ~cap bytes
                // ~125 times). A final trim below enforces the exact bound.
                if buf.len() > cap * 2 {
                    truncated |= trim_front_to_cap(&mut buf, cap);
                }
            }
        }
    }
    // The loop leaves up to 2x cap buffered; trim the tail down to the cap.
    truncated |= trim_front_to_cap(&mut buf, cap);
    (buf, truncated)
}

/// Drop the front of `buf` so at most `cap` bytes (the tail, where build/test
/// errors cluster) remain. Cuts on the next UTF-8 char boundary so a multi-byte
/// sequence is never split; otherwise `from_utf8_lossy` would corrupt the kept
/// output's first char. Returns whether any bytes were dropped.
fn trim_front_to_cap(buf: &mut Vec<u8>, cap: usize) -> bool {
    if buf.len() <= cap {
        return false;
    }
    let mut cut = buf.len() - cap;
    while cut < buf.len() && (buf[cut] & 0xC0) == 0x80 {
        cut += 1;
    }
    buf.drain(..cut);
    true
}

/// Turn captured bytes into a string, noting up front if earlier output was
/// dropped (we keep the tail).
fn finalize_stream((bytes, truncated): (Vec<u8>, bool)) -> String {
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        format!("... (earlier output truncated to at most {MAX_STREAM_BYTES} bytes)\n{text}")
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
    cancel: &CancellationToken,
) -> Result<ToolOutput> {
    let output = run_raw(program, args, cwd, timeout, cancel).await?;

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
        return Ok(ToolOutput::text(
            "(command completed successfully with no output)",
        ));
    }
    Ok(ToolOutput::text(sections.join("\n")))
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
        let out = run(
            "echo",
            vec!["hello".to_string()],
            None,
            DEFAULT_TIMEOUT,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.to_text(), "hello");
    }

    #[tokio::test]
    async fn test_run_reports_nonzero_exit() {
        // `sh -c 'exit 3'` completes (non-zero) — reported, not an error.
        let out = run(
            "sh",
            vec!["-c".to_string(), "exit 3".to_string()],
            None,
            DEFAULT_TIMEOUT,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let out = out.to_text();
        assert!(out.contains("exit code 3"), "{out}");
    }

    #[tokio::test]
    async fn test_run_missing_program_errors() {
        let err = run(
            "boitata_nope_xyz",
            vec![],
            None,
            DEFAULT_TIMEOUT,
            &CancellationToken::new(),
        )
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
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn test_run_cancelled_returns_promptly() {
        // A pre-cancelled token short-circuits the run: the child is spawned then
        // its group is killed, and the error says "cancelled" (not "timed out"),
        // well before the 30s timeout could fire.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = run(
            "sleep",
            vec!["30".to_string()],
            None,
            Duration::from_secs(30),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("cancelled"), "{err}");
    }

    #[tokio::test]
    async fn test_read_capped_no_truncation_no_duplication() {
        // Under the cap: bytes are kept verbatim, exactly once (regression for a
        // bug that appended each chunk twice).
        let data: Vec<u8> = (0..50u8).collect();
        let mut pipe = Some(&data[..]);
        let (kept, truncated) = read_capped(&mut pipe, 1000).await;
        assert!(!truncated);
        assert_eq!(kept, data);
    }

    #[tokio::test]
    async fn test_read_capped_keeps_tail() {
        // Over the cap: only the last `cap` bytes (the tail) survive. ASCII data
        // so the UTF-8 boundary walk doesn't shift the cut.
        let data: Vec<u8> = (0..200).map(|i| b'a' + (i % 26) as u8).collect();
        let mut pipe = Some(&data[..]);
        let (kept, truncated) = read_capped(&mut pipe, 40).await;
        assert!(truncated);
        assert_eq!(kept, data[160..]);
    }

    #[tokio::test]
    async fn test_read_capped_keeps_utf8_boundary() {
        // The kept tail must start on a char boundary so `from_utf8_lossy` doesn't
        // corrupt the first char. The accented letter below is two bytes
        // (0xC3 0xA9); cutting between them would split it.
        let data = "a\u{e9}".repeat(50).into_bytes(); // 150 bytes, accent = 2 bytes each
        let mut pipe = Some(&data[..]);
        let (kept, _truncated) = read_capped(&mut pipe, 40).await;
        assert!(
            std::str::from_utf8(&kept).is_ok(),
            "tail split a multi-byte char"
        );
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
