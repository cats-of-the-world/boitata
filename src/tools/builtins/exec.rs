// Shared subprocess execution for the command-oriented built-in tools
// (execute_command, search, git_*, cargo_*).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
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

/// Captured output of one stream: the kept tail, whether earlier bytes were
/// dropped, and — when they were — the path of a temp file holding the *full*,
/// untruncated stream for later inspection.
pub(super) struct CappedStream {
    bytes: Vec<u8>,
    truncated: bool,
    spill: Option<PathBuf>,
}

/// How a subprocess run ended: normally, by timeout, or by external cancellation.
/// The `Finished` payload is the joined `(stdout, stderr, wait-status)` triple.
type JoinedOutput = (
    CappedStream,
    CappedStream,
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
            let stdout = read_capped(&mut stdout_pipe, MAX_STREAM_BYTES, "stdout");
            let stderr = read_capped(&mut stderr_pipe, MAX_STREAM_BYTES, "stderr");
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
/// command can't exhaust memory. Continues reading past the cap (so the child
/// never blocks on a full pipe), but instead of discarding the overflow it
/// streams the *whole* output to a temp file named with `label` once the cap is
/// exceeded. The kept tail is for inline display (build/test errors cluster at
/// the end); the spill file preserves the middle the tail can't.
async fn read_capped<R: AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    cap: usize,
    label: &str,
) -> CappedStream {
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut spill: Option<(tokio::fs::File, PathBuf)> = None;
    // Once a spill write fails we stop trying: retrying after the buffer has been
    // trimmed would produce an incomplete file we'd wrongly report as the full
    // output. The stream then keeps only its in-memory tail.
    let mut spill_gave_up = false;
    let Some(reader) = pipe.as_mut() else {
        return CappedStream {
            bytes: buf,
            truncated,
            spill: None,
        };
    };
    let mut chunk = [0u8; 8192];
    loop {
        // A read error on a child pipe is treated as end-of-stream: the capture
        // is best-effort and the child's exit status is the source of truth for
        // success/failure, so we keep whatever we have rather than fail the tool.
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = &chunk[..n];
                // Already spilling: persist this chunk before it can be trimmed.
                // If the write fails (e.g. disk full), discard the partial spill
                // and give up — a partial spill is worse than none.
                let write_failed = match spill.as_mut() {
                    Some((file, _)) => file.write_all(data).await.is_err(),
                    None => false,
                };
                if write_failed {
                    spill_gave_up = true;
                    if let Some((_, path)) = spill.take() {
                        tracing::warn!(
                            "output spill write failed; discarding partial spill file {} (tail only)",
                            path.display()
                        );
                        let _ = tokio::fs::remove_file(&path).await;
                    }
                }
                buf.extend_from_slice(data);
                // Only trim once the buffer reaches 2x the cap, so the O(cap)
                // memmove is amortized over many chunks instead of running on
                // every read. A final trim below enforces the exact bound.
                if buf.len() > cap * 2 {
                    // First overflow: start spilling by writing everything read
                    // so far. Nothing has been spilled yet, so `buf` holds the
                    // complete stream up to this point; subsequent chunks are
                    // written above before they can be trimmed.
                    if spill.is_none() && !spill_gave_up {
                        match create_spill(label).await {
                            Some((mut file, path)) => {
                                if file.write_all(&buf).await.is_ok() {
                                    spill = Some((file, path));
                                } else {
                                    // The initial write failed; give up rather
                                    // than retry from an already-trimmed buffer.
                                    spill_gave_up = true;
                                    tracing::warn!(
                                        "output spill write failed; discarding spill file {} (tail only)",
                                        path.display()
                                    );
                                    let _ = tokio::fs::remove_file(&path).await;
                                }
                            }
                            None => spill_gave_up = true,
                        }
                    }
                    truncated |= trim_front_to_cap(&mut buf, cap);
                }
            }
        }
    }
    // If the output ended between `cap` and `2*cap` bytes we never crossed the
    // in-loop spill threshold, yet the final trim below still drops the front.
    // `buf` here holds the complete stream (nothing was trimmed while it stayed
    // under 2*cap), so spill it before trimming rather than lose those bytes.
    if buf.len() > cap && spill.is_none() && !spill_gave_up {
        if let Some((mut file, path)) = create_spill(label).await {
            if file.write_all(&buf).await.is_ok() {
                spill = Some((file, path));
            } else {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    // The loop leaves up to 2x cap buffered; trim the tail down to the cap.
    truncated |= trim_front_to_cap(&mut buf, cap);
    let spill = match spill {
        Some((mut file, path)) => {
            // A failed flush may mean a truncated file; drop it rather than
            // report an incomplete spill as the full output.
            if file.flush().await.is_err() {
                tracing::warn!(
                    "output spill flush failed; discarding spill file {}",
                    path.display()
                );
                let _ = tokio::fs::remove_file(&path).await;
                None
            } else {
                Some(path)
            }
        }
        None => None,
    };
    CappedStream {
        bytes: buf,
        truncated,
        spill,
    }
}

/// Distinctive prefix for command-output spill files, so the stale-file sweep
/// only ever touches our own spills (not, say, a `boitata-audit.log`).
const SPILL_PREFIX: &str = "boitata-cmdout-";
/// Spill files left by earlier runs are pruned once they exceed this age.
const SPILL_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Create a temp file to hold a stream's full output. Best-effort: on failure we
/// log and return `None`, keeping only the in-memory tail. The file is left in
/// place (not auto-deleted) so it can be inspected after the run; stale ones are
/// swept by [`cleanup_stale_spills`].
async fn create_spill(label: &str) -> Option<(tokio::fs::File, PathBuf)> {
    // Prune stale spills from earlier runs on first use so they don't accumulate
    // unbounded. Runs at most once per process, on the blocking pool so its
    // synchronous filesystem I/O doesn't stall the async worker.
    static CLEANUP: std::sync::Once = std::sync::Once::new();
    CLEANUP.call_once(|| {
        tokio::task::spawn_blocking(cleanup_stale_spills);
    });

    let path = std::env::temp_dir().join(format!(
        "{SPILL_PREFIX}{label}-{}.log",
        uuid::Uuid::new_v4()
    ));
    // Command output can contain sensitive data, and the temp dir is shared, so
    // create the spill readable/writable by the owner only (0600) on Unix.
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    // `tokio::fs::OpenOptions::mode` is an inherent method on Unix (no import).
    #[cfg(unix)]
    opts.mode(0o600);
    match opts.open(&path).await {
        Ok(file) => Some((file, path)),
        Err(e) => {
            tracing::warn!("could not create output spill file {}: {e}", path.display());
            None
        }
    }
}

/// Best-effort removal of spill files from earlier runs older than
/// [`SPILL_MAX_AGE`]. Ignores every error (missing dir, permission, races) — a
/// failed sweep must never affect the current command.
fn cleanup_stale_spills() {
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(SPILL_PREFIX)
        {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > SPILL_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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

/// Turn a captured stream into a string. When the output was truncated, note it
/// up front (we keep the tail) and point at the spill file holding the full
/// output when one was written.
fn finalize_stream(stream: CappedStream) -> String {
    let CappedStream {
        bytes,
        truncated,
        spill,
    } = stream;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if !truncated {
        return text;
    }
    match spill {
        Some(path) => format!(
            "... (output truncated to the last {MAX_STREAM_BYTES} bytes; full output saved to {})\n{text}",
            path.display()
        ),
        None => {
            format!("... (earlier output truncated to at most {MAX_STREAM_BYTES} bytes)\n{text}")
        }
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
        // bug that appended each chunk twice), and nothing is spilled.
        let data: Vec<u8> = (0..50u8).collect();
        let mut pipe = Some(&data[..]);
        let out = read_capped(&mut pipe, 1000, "stdout").await;
        assert!(!out.truncated);
        assert!(out.spill.is_none());
        assert_eq!(out.bytes, data);
    }

    #[tokio::test]
    async fn test_read_capped_keeps_tail() {
        // Over the cap: only the last `cap` bytes (the tail) survive in memory.
        // ASCII data so the UTF-8 boundary walk doesn't shift the cut.
        let data: Vec<u8> = (0..200).map(|i| b'a' + (i % 26) as u8).collect();
        let mut pipe = Some(&data[..]);
        let out = read_capped(&mut pipe, 40, "stdout").await;
        assert!(out.truncated);
        assert_eq!(out.bytes, data[160..]);
    }

    #[tokio::test]
    async fn test_read_capped_spills_full_output() {
        // Over the cap: the full stream is written to a spill file, even though
        // only the tail is kept in memory.
        let data: Vec<u8> = (0..5000).map(|i| b'a' + (i % 26) as u8).collect();
        let mut pipe = Some(&data[..]);
        let out = read_capped(&mut pipe, 100, "stdout").await;
        assert!(out.truncated);
        assert_eq!(out.bytes.len(), 100);
        let path = out.spill.expect("a spill file should have been written");
        let spilled = std::fs::read(&path).expect("read spill file");
        assert_eq!(spilled, data, "spill file must hold the complete output");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_read_capped_spills_when_output_between_cap_and_2x() {
        // Output in (cap, 2*cap] never crosses the in-loop spill threshold, but
        // the tail is still trimmed — so it must be spilled at the end, not lost.
        let data: Vec<u8> = (0..150).map(|i| b'a' + (i % 26) as u8).collect();
        let mut pipe = Some(&data[..]);
        let out = read_capped(&mut pipe, 100, "stdout").await;
        assert!(out.truncated);
        assert_eq!(out.bytes.len(), 100);
        let path = out.spill.expect("output past the cap must be spilled");
        let spilled = std::fs::read(&path).expect("read spill file");
        assert_eq!(spilled, data, "spill file must hold the complete output");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_read_capped_keeps_utf8_boundary() {
        // The kept tail must start on a char boundary so `from_utf8_lossy` doesn't
        // corrupt the first char. The accented letter below is two bytes
        // (0xC3 0xA9); cutting between them would split it.
        let data = "a\u{e9}".repeat(50).into_bytes(); // 150 bytes, accent = 2 bytes each
        let mut pipe = Some(&data[..]);
        let out = read_capped(&mut pipe, 40, "stdout").await;
        assert!(
            std::str::from_utf8(&out.bytes).is_ok(),
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
