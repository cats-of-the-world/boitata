//! Run commands inside a Firecracker guest over SSH.
//!
//! A microVM has no Docker-style exec API, so the guest runs `sshd` (with the
//! ephemeral key we inject via MMDS) and the host reaches it on the TAP address.
//! We shell out to the system `ssh` — the same "drive a CLI tool" approach the
//! agent's git/cargo tools already use — rather than pulling in an SSH library.
//!
//! [`exec_argv`] (which builds the `ssh` command line, with the remote command
//! shell-quoted so a node's output can't inject) is pure and unit-tested; running
//! it needs a booted VM and is verified on a KVM host. Output is capped at
//! [`crate::sandbox::MAX_EXEC_OUTPUT`].

use std::time::Duration;

use anyhow::Context;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::sandbox::append_capped;

/// SSH connection options: fail fast, don't prompt, and don't pollute or consult
/// the host's `known_hosts` (each VM has a throwaway host key).
const SSH_OPTS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "LogLevel=ERROR",
];

/// Build the `ssh` argv to run `argv` (optionally under `workdir`) on the guest.
/// `argv` is joined into a single remote command with each word shell-quoted, so
/// a value carrying shell metacharacters is passed literally, not interpreted by
/// the remote shell.
pub fn exec_argv(
    key_path: &str,
    user: &str,
    host: &str,
    argv: &[String],
    workdir: Option<&str>,
) -> Vec<String> {
    let mut remote = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(wd) = workdir {
        remote = format!("cd {} && {}", shell_quote(wd), remote);
    }

    let mut out = vec!["ssh".to_string(), "-i".to_string(), key_path.to_string()];
    out.extend(SSH_OPTS.iter().map(|s| s.to_string()));
    out.push(format!("{user}@{host}"));
    // The remote command is one final argument (already a complete shell command).
    out.push(remote);
    out
}

/// Run `argv` on the guest over SSH, returning `(exit_code, combined output)`.
/// Output (stdout then stderr) is captured and capped at [`MAX_EXEC_OUTPUT`]. A
/// cancellation kills the local `ssh` process, which drops the connection.
pub async fn exec(
    key_path: &str,
    user: &str,
    host: &str,
    argv: &[String],
    workdir: Option<&str>,
    cancel: &CancellationToken,
) -> anyhow::Result<(i64, String)> {
    let ssh = exec_argv(key_path, user, host, argv, workdir);
    let (program, args) = ssh.split_first().expect("exec_argv is never empty");

    let child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to launch ssh to {user}@{host}"))?;

    // `wait_with_output` owns the child; on cancellation we drop that future,
    // which drops the child — and `kill_on_drop` stops the local ssh process,
    // tearing down the connection.
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let output = tokio::select! {
        _ = cancel.cancelled() => anyhow::bail!("cancelled during ssh exec on {host}"),
        output = &mut wait => output.context("failed to wait for ssh")?,
    };

    let mut combined = String::new();
    let mut truncated = false;
    append_capped(
        &mut combined,
        &String::from_utf8_lossy(&output.stdout),
        &mut truncated,
    );
    append_capped(
        &mut combined,
        &String::from_utf8_lossy(&output.stderr),
        &mut truncated,
    );
    if truncated {
        combined.push_str("\n…[output truncated]");
    }

    // ssh propagates the remote command's exit code; 255 means ssh itself failed
    // (e.g. connection refused), which surfaces as a non-zero status to the caller.
    let code = output.status.code().unwrap_or(255) as i64;
    Ok((code, combined))
}

/// Wait (up to `timeout`) for a fresh guest to become reachable over SSH by
/// running `true` on it, so exec/checkout nodes don't race sshd's startup.
pub async fn wait_ready(
    key_path: &str,
    user: &str,
    host: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let probe = vec!["true".to_string()];
    loop {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled while waiting for ssh on {host}");
        }
        match exec(key_path, user, host, &probe, None, cancel).await {
            Ok((0, _)) => return Ok(()),
            _ if std::time::Instant::now() >= deadline => {
                anyhow::bail!("ssh on {host} did not become ready within {timeout:?}");
            }
            _ => {
                tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("cancelled while waiting for ssh on {host}"),
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                }
            }
        }
    }
}

/// Single-quote a value for safe inclusion in a remote `sh` command.
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exec_argv_builds_ssh_command_with_quoted_remote() {
        let cmd = exec_argv(
            "/keys/id",
            "root",
            "172.16.5.2",
            &argv(&["sh", "-c", "git clone x"]),
            None,
        );
        assert_eq!(cmd[0], "ssh");
        assert_eq!(cmd[1], "-i");
        assert_eq!(cmd[2], "/keys/id");
        assert!(cmd.contains(&"StrictHostKeyChecking=no".to_string()));
        assert_eq!(cmd[cmd.len() - 2], "root@172.16.5.2");
        assert_eq!(cmd[cmd.len() - 1], "'sh' '-c' 'git clone x'");
    }

    #[test]
    fn exec_argv_prepends_workdir() {
        let cmd = exec_argv("/k", "root", "h", &argv(&["ls"]), Some("/workspace"));
        assert_eq!(cmd.last().unwrap(), "cd '/workspace' && 'ls'");
    }

    #[test]
    fn exec_argv_quotes_injection_attempts() {
        // A remote word containing shell metacharacters is a single quoted token.
        let cmd = exec_argv("/k", "root", "h", &argv(&["sh", "-c", "; rm -rf /"]), None);
        assert_eq!(cmd.last().unwrap(), "'sh' '-c' '; rm -rf /'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
