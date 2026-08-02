//! The Docker backend for [`Sandbox`](super::Sandbox): each sandbox is an
//! ephemeral container. The daemon client connects lazily on first use.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{ContainerCreateBody, ExecConfig};
use bollard::query_parameters::{CreateImageOptionsBuilder, RemoveContainerOptionsBuilder};
use futures::StreamExt;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use super::Sandbox;

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

        // If starting fails, remove the container we just created so `provision`
        // never leaves an orphan behind.
        if let Err(e) = docker.start_container(&created.id, None).await {
            let _ = self.destroy(&created.id).await;
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
