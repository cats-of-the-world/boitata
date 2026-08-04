//! Sandbox-provisioning nodes.
//!
//! These nodes let a blueprint move execution off the host into an isolated
//! sandbox: [`ProvisionNode`] creates one, [`CheckoutNode`] git-clones a repo into
//! it, and [`ExecNode`] runs commands inside it. The sandbox backend (Docker
//! today) lives behind the [`Sandbox`](super::sandbox::Sandbox) trait; every
//! sandbox a run provisions is tracked by [`Sandboxes`](super::sandbox::Sandboxes)
//! and destroyed by the executor when the run ends — see
//! `Executor::run_with_cancel`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::nodes::{Node, NodeCtx};
use super::state::{State, Status, Update, render};
use boitata_core::audit::{AuditSink, NodeKind};
use tokio_util::sync::CancellationToken;

/// Where a checkout clones by default (inside the sandbox).
const DEFAULT_WORKSPACE: &str = "/workspace";
/// Default port the in-sandbox agent listens on for ACP.
const DEFAULT_AGENT_PORT: u16 = 9000;
/// Default command that starts the ACP agent inside the sandbox.
const DEFAULT_AGENT_COMMAND: &str = "boitata-agent";
/// How long to wait for the in-sandbox agent to start accepting connections.
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Build the shell command a [`CheckoutNode`] runs: clone `repo` into `path`, then
/// (optionally) check out `git_ref`. Kept pure so it can be unit-tested without a
/// daemon. Values are passed as `sh -c` arguments with the parts quoted.
fn checkout_command(repo: &str, path: &str, git_ref: Option<&str>) -> Vec<String> {
    let mut script = format!("git clone {} {}", shell_quote(repo), shell_quote(path));
    if let Some(git_ref) = git_ref {
        script.push_str(&format!(
            " && git -C {} checkout {}",
            shell_quote(path),
            shell_quote(git_ref)
        ));
    }
    vec!["sh".into(), "-c".into(), script]
}

/// Single-quote a value for safe inclusion in a `sh -c` script.
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

/// Create an ephemeral container from `image`. Its output (the container id) is
/// stored under the node name, so downstream nodes reference it as `{name}`.
pub struct ProvisionNode {
    name: String,
    image: String,
}

impl ProvisionNode {
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
        }
    }
}

#[async_trait]
impl Node for ProvisionNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> NodeKind {
        NodeKind::Container
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let image = render(&self.image, state);
        let id = cx.sandbox.provision(&image, &cx.cancel).await?;
        Ok(Update::from_node(&self.name, id, Status::Ok))
    }
}

/// Git-clone `repo` into a container (referenced by `container`, e.g. `{box}`).
/// Routes on the clone's exit code.
pub struct CheckoutNode {
    name: String,
    container: String,
    repo: String,
    git_ref: Option<String>,
    path: String,
}

impl CheckoutNode {
    pub fn new(
        name: impl Into<String>,
        container: impl Into<String>,
        repo: impl Into<String>,
        git_ref: Option<String>,
        path: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            container: container.into(),
            repo: repo.into(),
            git_ref,
            path: path.unwrap_or_else(|| DEFAULT_WORKSPACE.to_string()),
        }
    }
}

#[async_trait]
impl Node for CheckoutNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> NodeKind {
        NodeKind::Container
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let container = render(&self.container, state);
        let repo = render(&self.repo, state);
        let git_ref = self.git_ref.as_ref().map(|r| render(r, state));
        let argv = checkout_command(&repo, &self.path, git_ref.as_deref());
        let (code, output) = cx.sandbox.exec(&container, argv, None, &cx.cancel).await?;
        let status = if code == 0 {
            Status::Ok
        } else {
            Status::Failed
        };
        Ok(Update::from_node(&self.name, output, status))
    }
}

/// Run a shell command inside a container (referenced by `container`). Routes on
/// the command's exit code, mirroring `ScriptNode` but in the container.
pub struct ExecNode {
    name: String,
    container: String,
    run: String,
    workdir: Option<String>,
}

impl ExecNode {
    pub fn new(
        name: impl Into<String>,
        container: impl Into<String>,
        run: impl Into<String>,
        workdir: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            container: container.into(),
            run: run.into(),
            workdir,
        }
    }
}

#[async_trait]
impl Node for ExecNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> NodeKind {
        NodeKind::Container
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let container = render(&self.container, state);
        let command = render(&self.run, state);
        let argv = vec!["sh".to_string(), "-c".to_string(), command];
        let (code, output) = cx
            .sandbox
            .exec(&container, argv, self.workdir.as_deref(), &cx.cancel)
            .await?;
        let status = if code == 0 {
            Status::Ok
        } else {
            Status::Failed
        };
        Ok(Update::from_node(&self.name, output, status))
    }
}

/// Run an agent *inside* a sandbox over ACP: launch the `boitata-agent` server
/// in the container, connect to it, and stream its events into the blueprint —
/// the containerized counterpart of [`AgentNode`](super::nodes::AgentNode).
pub struct AgentSandboxNode {
    name: String,
    container: String,
    prompt: String,
    port: u16,
    command: String,
}

impl AgentSandboxNode {
    /// In-sandbox file holding the launched agent's PID, for later cleanup.
    const PID_FILE: &'static str = "/tmp/boitata-agent.pid";

    pub fn new(
        name: impl Into<String>,
        container: impl Into<String>,
        prompt: impl Into<String>,
        port: Option<u16>,
        command: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            container: container.into(),
            prompt: prompt.into(),
            port: port.unwrap_or(DEFAULT_AGENT_PORT),
            command: command.unwrap_or_else(|| DEFAULT_AGENT_COMMAND.to_string()),
        }
    }

    /// The shell command that starts the agent detached inside the sandbox.
    /// `nohup … &` keeps it running after the exec session returns; its PID is
    /// recorded so [`stop_command`](Self::stop_command) can stop it afterward. The
    /// command (from blueprint YAML) is shell-quoted so it's a single program word
    /// and can't inject into the `sh -c` script; the port is a `u16`, so it's
    /// already safe to interpolate.
    fn launch_command(&self) -> Vec<String> {
        let script = format!(
            "nohup {} --addr 0.0.0.0:{} >/tmp/boitata-agent.log 2>&1 & echo $! >{}",
            shell_quote(&self.command),
            self.port,
            Self::PID_FILE,
        );
        vec!["sh".into(), "-c".into(), script]
    }

    /// Best-effort command to stop the detached agent launched above, so it
    /// doesn't linger inside the sandbox after the turn (a single sandbox may run
    /// several `agent_sandbox` nodes in sequence). `|| true` keeps a missing
    /// pidfile or already-exited process from failing the exec.
    fn stop_command() -> Vec<String> {
        let script = format!(
            "kill \"$(cat {pid} 2>/dev/null)\" 2>/dev/null || true",
            pid = Self::PID_FILE,
        );
        vec!["sh".into(), "-c".into(), script]
    }
}

#[async_trait]
impl Node for AgentSandboxNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> NodeKind {
        NodeKind::Agent
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let container = render(&self.container, state);
        let prompt = render(&self.prompt, state);

        // 1. Launch the ACP agent inside the sandbox (returns immediately).
        cx.sandbox
            .exec(&container, self.launch_command(), None, &cx.cancel)
            .await?;

        // 2. Resolve the sandbox's address and wait for the agent to come up.
        let addr = cx.sandbox.endpoint(&container, self.port).await?;
        wait_ready(&addr, &cx.cancel).await?;

        // 3. Drive it over ACP, teeing the agent's events into the blueprint's
        //    audit stream (so they surface exactly like a local agent's).
        let sink: Arc<dyn AuditSink> = cx
            .audit
            .clone()
            .unwrap_or_else(|| Arc::new(NoopSink) as Arc<dyn AuditSink>);
        let result = boitata_acp::run_prompt(&addr, prompt, sink, cx.cancel.clone()).await;

        // 4. Stop the detached agent regardless of how the turn ended (success,
        //    failure, or cancellation), so it doesn't outlive the turn. Use a
        //    fresh, uncancelled token so cleanup still runs after a cancelled run;
        //    it's best-effort, so a failure here (e.g. the sandbox already gone) is
        //    ignored.
        let _ = cx
            .sandbox
            .exec(
                &container,
                Self::stop_command(),
                None,
                &CancellationToken::new(),
            )
            .await;

        let outcome = result?;
        let status = if outcome.success {
            Status::Ok
        } else {
            Status::Failed
        };
        let text = outcome.message.unwrap_or_else(|| {
            if outcome.success {
                format!("agent `{}` produced no output", self.name)
            } else {
                format!("agent `{}` failed", self.name)
            }
        });
        Ok(Update::from_node(&self.name, text, status))
    }
}

/// Poll `addr` until it accepts a TCP connection or [`AGENT_READY_TIMEOUT`]
/// elapses, so we don't race the agent's startup.
async fn wait_ready(addr: &str, cancel: &CancellationToken) -> anyhow::Result<()> {
    let deadline = Instant::now() + AGENT_READY_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled while waiting for the agent at {addr}");
        }
        match tokio::net::TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(e) if Instant::now() >= deadline => {
                return Err(e).map_err(|e| {
                    anyhow::anyhow!("agent at {addr} did not become ready in time: {e}")
                });
            }
            // Back off before retrying, but wake immediately on cancellation
            // instead of sleeping out the full interval first.
            Err(_) => {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        anyhow::bail!("cancelled while waiting for the agent at {addr}");
                    }
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
            }
        }
    }
}

/// An [`AuditSink`] that discards events, used when the run has no audit sink.
struct NoopSink;
impl AuditSink for NoopSink {
    fn record(&self, _event: boitata_core::audit::AuditEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_command_clones_into_path() {
        let argv = checkout_command("https://example.com/r.git", "/workspace", None);
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        assert_eq!(
            argv[2],
            "git clone 'https://example.com/r.git' '/workspace'"
        );
    }

    #[test]
    fn checkout_command_checks_out_ref() {
        let argv = checkout_command("r", "/w", Some("v1.2"));
        assert_eq!(argv[2], "git clone 'r' '/w' && git -C '/w' checkout 'v1.2'");
    }

    #[test]
    fn shell_quote_neutralizes_injection() {
        // A repo/ref containing shell metacharacters is quoted, not interpreted.
        let argv = checkout_command("; rm -rf /", "/w", None);
        assert_eq!(argv[2], "git clone '; rm -rf /' '/w'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn launch_command_quotes_the_agent_command() {
        // The `command` field comes from blueprint YAML; a value with shell
        // metacharacters must be a single quoted word, not an injection into the
        // `sh -c` launch script.
        let node = AgentSandboxNode::new(
            "agent",
            "{box}",
            "{task}",
            Some(9000),
            Some("; rm -rf / #".to_string()),
        );
        let argv = node.launch_command();
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        assert_eq!(
            argv[2],
            "nohup '; rm -rf / #' --addr 0.0.0.0:9000 >/tmp/boitata-agent.log 2>&1 & \
             echo $! >/tmp/boitata-agent.pid"
        );
    }
}
