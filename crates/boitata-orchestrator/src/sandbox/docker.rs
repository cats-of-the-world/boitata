//! The Docker backend for [`Sandbox`](super::Sandbox): each sandbox is an
//! ephemeral container. The daemon client connects lazily on first use.

use std::time::Duration;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{ContainerCreateBody, ExecConfig};
use bollard::query_parameters::{CreateImageOptionsBuilder, RemoveContainerOptionsBuilder};
use futures::StreamExt;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use super::Sandbox;

/// Give up on an image pull that stalls this long, so a slow registry or a
/// hung connection can't keep a provision future alive forever.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// Cap on captured exec output. A verbose command shouldn't be able to OOM the
/// orchestrator; output past this is dropped with a trailing marker.
const MAX_EXEC_OUTPUT: usize = 1 << 20; // 1 MiB

/// A [`Sandbox`] backed by local Docker containers.
#[derive(Default)]
pub struct DockerSandbox {
    docker: OnceCell<Docker>,
}

impl DockerSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect to the local daemon on first use. Errors surface here (rather than
    /// at construction) so runs without sandbox nodes never need Docker.
    async fn docker(&self) -> anyhow::Result<&Docker> {
        self.docker
            .get_or_try_init(|| async {
                Docker::connect_with_local_defaults()
                    .context("failed to connect to the Docker daemon")
            })
            .await
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn provision(&self, image: &str, cancel: &CancellationToken) -> anyhow::Result<String> {
        let docker = self.docker().await?;

        // Pull the image (a no-op layer-wise if already present). Draining the
        // stream to completion is what actually performs the pull; a stall is
        // bounded by both the cancel token and PULL_TIMEOUT.
        let options = CreateImageOptionsBuilder::new().from_image(image).build();
        let pull = async {
            let mut stream = docker.create_image(Some(options), None, None);
            while let Some(item) = stream.next().await {
                item.with_context(|| format!("failed to pull image `{image}`"))?;
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("cancelled while pulling `{image}`"),
            result = tokio::time::timeout(PULL_TIMEOUT, pull) => {
                result.with_context(|| format!("timed out pulling image `{image}`"))??;
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

        // If starting fails, remove the container we just created so `provision`
        // never leaves an orphan behind. If that removal also fails (e.g. the
        // daemon went away), log it — there's nothing left to record it against.
        if let Err(e) = docker.start_container(&created.id, None).await {
            if let Err(cleanup) = self.destroy(&created.id).await {
                tracing::warn!(
                    "failed to remove container {} after a failed start: {cleanup}",
                    created.id
                );
            }
            return Err(e).with_context(|| format!("failed to start container {}", created.id));
        }
        Ok(created.id)
    }

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
        let mut truncated = false;
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
                    // Docker has no "kill exec" API; the exec process is reaped
                    // when the sandbox is force-removed at run end (cleanup_all),
                    // which cancellation ultimately triggers.
                    _ = cancel.cancelled() => anyhow::bail!("cancelled during exec"),
                    chunk = stream.next() => match chunk {
                        Some(chunk) => {
                            let chunk = chunk.context("exec stream error")?.to_string();
                            // Keep reading to the end (for the exit code) but stop
                            // accumulating once we hit the cap.
                            if output.len() < MAX_EXEC_OUTPUT {
                                output.push_str(&chunk);
                            } else {
                                truncated = true;
                            }
                        }
                        None => break,
                    },
                }
            }
        }
        if truncated {
            output.push_str("\n…[output truncated]");
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

    async fn destroy(&self, id: &str) -> anyhow::Result<()> {
        let docker = self.docker().await?;
        let options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();
        docker
            .remove_container(id, Some(options))
            .await
            .with_context(|| format!("failed to remove container {id}"))
    }
}
