//! Container-provisioning nodes and the per-run container manager.
//!
//! These nodes let a blueprint move execution off the host into an ephemeral
//! Docker container: [`ProvisionNode`] creates one, [`CheckoutNode`] git-clones a
//! repo into it, and [`ExecNode`] runs commands inside it. Every container a run
//! provisions is tracked by [`Containers`] and destroyed by the executor when the
//! run ends (success, failure, or cancel) — see `Executor::run_with_cancel`.
//!
//! The Docker client connects lazily on first use, so a blueprint that uses no
//! container nodes never requires a running daemon.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{ContainerCreateBody, ExecConfig};
use bollard::query_parameters::{CreateImageOptionsBuilder, RemoveContainerOptionsBuilder};
use futures::StreamExt;
use tokio::sync::{Mutex, OnceCell};
use tokio_util::sync::CancellationToken;

use super::nodes::{Node, NodeCtx};
use super::state::{State, Status, Update, render};
use boitata_core::audit::NodeKind;

/// Where a checkout clones by default (inside the container).
const DEFAULT_WORKSPACE: &str = "/workspace";

/// Per-run Docker handle: a lazily-connected client plus the ids of every
/// container the run has provisioned, so they can all be torn down at the end.
#[derive(Default)]
pub struct Containers {
    docker: OnceCell<Docker>,
    provisioned: Mutex<Vec<String>>,
}

impl Containers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect to the local daemon on first use. Errors surface here (rather than
    /// at construction) so runs without container nodes never need Docker.
    async fn docker(&self) -> anyhow::Result<&Docker> {
        self.docker
            .get_or_try_init(|| async {
                Docker::connect_with_local_defaults()
                    .context("failed to connect to the Docker daemon")
            })
            .await
    }

    /// Pull `image` if needed, create a container running the keep-alive command,
    /// start it, record its id for cleanup, and return the id.
    async fn provision(&self, image: &str, cancel: &CancellationToken) -> anyhow::Result<String> {
        let docker = self.docker().await?;

        // Pull the image (a no-op layer-wise if already present). Draining the
        // stream to completion is what actually performs the pull.
        let options = CreateImageOptionsBuilder::new().from_image(image).build();
        let mut pull = docker.create_image(Some(options), None, None);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => anyhow::bail!("cancelled while pulling `{image}`"),
                next = pull.next() => match next {
                    Some(item) => { item.with_context(|| format!("failed to pull image `{image}`"))?; }
                    None => break,
                },
            }
        }

        // Keep the container alive so we can `exec` into it. Override the
        // entrypoint too (not just cmd), so an image with its own ENTRYPOINT
        // (e.g. `alpine/git`) doesn't turn this into `<entrypoint> sleep infinity`
        // and exit immediately.
        let body = ContainerCreateBody {
            image: Some(image.to_string()),
            entrypoint: Some(vec!["sleep".to_string()]),
            cmd: Some(vec!["infinity".to_string()]),
            ..Default::default()
        };
        let created = docker
            .create_container(None, body)
            .await
            .with_context(|| format!("failed to create container from `{image}`"))?;
        docker
            .start_container(&created.id, None)
            .await
            .with_context(|| format!("failed to start container {}", created.id))?;

        self.provisioned.lock().await.push(created.id.clone());
        Ok(created.id)
    }

    /// Run `argv` inside container `id`, returning `(exit_code, combined output)`.
    /// Respects `cancel` (the exec is dropped if the run is cancelled).
    async fn exec(
        &self,
        id: &str,
        argv: Vec<String>,
        workdir: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(i64, String)> {
        let docker = self.docker().await?;
        let config = ExecConfig {
            cmd: Some(argv),
            working_dir: workdir.map(str::to_string),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };
        let exec = docker
            .create_exec(id, config)
            .await
            .context("failed to create exec")?;

        let mut output = String::new();
        let started = docker
            .start_exec(&exec.id, None)
            .await
            .context("failed to start exec")?;
        if let bollard::exec::StartExecResults::Attached {
            output: mut stream, ..
        } = started
        {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("cancelled during exec"),
                    chunk = stream.next() => match chunk {
                        Some(chunk) => output.push_str(&chunk.context("exec stream error")?.to_string()),
                        None => break,
                    },
                }
            }
        }

        let inspect = docker
            .inspect_exec(&exec.id)
            .await
            .context("failed to inspect exec")?;
        let code = inspect
            .exit_code
            .ok_or_else(|| anyhow!("exec produced no exit code"))?;
        Ok((code, output))
    }

    /// Force-remove every provisioned container. Best-effort: failures are logged,
    /// never propagated, so cleanup can't mask the run's own outcome.
    pub async fn cleanup_all(&self) {
        let ids = std::mem::take(&mut *self.provisioned.lock().await);
        if ids.is_empty() {
            return;
        }
        let Ok(docker) = self.docker().await else {
            return;
        };
        let options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();
        for id in ids {
            if let Err(e) = docker.remove_container(&id, Some(options.clone())).await {
                tracing::warn!("failed to remove container {id}: {e}");
            } else {
                tracing::info!("removed container {id}");
            }
        }
    }
}

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
        let id = cx.containers.provision(&image, &cx.cancel).await?;
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
        let (code, output) = cx
            .containers
            .exec(&container, argv, None, &cx.cancel)
            .await?;
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
            .containers
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
}
