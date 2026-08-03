//! Isolated execution environments for the provisioning nodes.
//!
//! A [`Sandbox`] is the backend contract — create an environment, run commands in
//! it, destroy it. Today the only backend is [`docker::DockerSandbox`]; a
//! Firecracker microVM backend is intended to slot in behind the same trait.
//!
//! [`Sandboxes`] is the per-run manager the executor and nodes hold: it delegates
//! to a backend and tracks every sandbox provisioned during the run so they can
//! all be torn down when it ends (see `Executor::run_with_cancel`). The backend
//! connects lazily, so a blueprint that provisions nothing never touches Docker.

mod docker;

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub use docker::DockerSandbox;

/// A backend that can create isolated environments, run commands in them, and
/// destroy them. `image` is an opaque "what to boot" spec the backend
/// interprets (Docker: an image name); it may widen to a struct for backends
/// (e.g. Firecracker) that need a kernel + rootfs.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Create and start an environment, returning an opaque id. Must not leave a
    /// half-provisioned environment behind on failure.
    async fn provision(&self, image: &str, cancel: &CancellationToken) -> anyhow::Result<String>;

    /// Run `argv` inside sandbox `id`, returning `(exit_code, combined output)`.
    /// Output is captured into memory; implementations should cap it so a verbose
    /// command can't exhaust the host (the Docker backend caps at 1 MiB).
    async fn exec(
        &self,
        id: &str,
        argv: Vec<String>,
        workdir: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(i64, String)>;

    /// Destroy sandbox `id`.
    async fn destroy(&self, id: &str) -> anyhow::Result<()>;
}

/// Per-run sandbox manager: delegates to a [`Sandbox`] backend and records every
/// environment provisioned so [`cleanup_all`](Self::cleanup_all) can destroy them
/// all when the run ends.
pub struct Sandboxes {
    backend: Arc<dyn Sandbox>,
    provisioned: Mutex<Vec<String>>,
}

impl Sandboxes {
    pub fn new(backend: Arc<dyn Sandbox>) -> Self {
        Self {
            backend,
            provisioned: Mutex::new(Vec::new()),
        }
    }

    /// The default backend: local Docker.
    pub fn with_docker() -> Self {
        Self::new(Arc::new(DockerSandbox::new()))
    }

    /// Provision an environment and record it for cleanup. The backend guarantees
    /// no orphan on failure, so only successfully-provisioned ids are tracked.
    pub async fn provision(
        &self,
        image: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let id = self.backend.provision(image, cancel).await?;
        self.provisioned.lock().await.push(id.clone());
        Ok(id)
    }

    pub async fn exec(
        &self,
        id: &str,
        argv: Vec<String>,
        workdir: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(i64, String)> {
        self.backend.exec(id, argv, workdir, cancel).await
    }

    /// Destroy every provisioned environment, concurrently. Best-effort: failures
    /// are logged, never propagated, so cleanup can't mask the run's own outcome.
    pub async fn cleanup_all(&self) {
        let ids = std::mem::take(&mut *self.provisioned.lock().await);
        let backend = &self.backend;
        futures::future::join_all(ids.into_iter().map(|id| async move {
            match backend.destroy(&id).await {
                Ok(()) => tracing::info!("destroyed sandbox {id}"),
                Err(e) => tracing::warn!("failed to destroy sandbox {id}: {e}"),
            }
        }))
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake backend that records the ids it provisioned and destroyed, so we can
    /// assert the manager's tracking/cleanup without Docker.
    #[derive(Default)]
    struct FakeSandbox {
        counter: AtomicUsize,
        destroyed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        async fn provision(&self, image: &str, _c: &CancellationToken) -> anyhow::Result<String> {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            Ok(format!("{image}-{n}"))
        }
        async fn exec(
            &self,
            _id: &str,
            _argv: Vec<String>,
            _workdir: Option<&str>,
            _c: &CancellationToken,
        ) -> anyhow::Result<(i64, String)> {
            Ok((0, String::new()))
        }
        async fn destroy(&self, id: &str) -> anyhow::Result<()> {
            self.destroyed.lock().await.push(id.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn cleanup_destroys_every_provisioned_sandbox() {
        let backend = Arc::new(FakeSandbox::default());
        let sandboxes = Sandboxes::new(backend.clone());
        let cancel = CancellationToken::new();

        let a = sandboxes.provision("img", &cancel).await.unwrap();
        let b = sandboxes.provision("img", &cancel).await.unwrap();
        sandboxes.cleanup_all().await;

        let mut destroyed = backend.destroyed.lock().await.clone();
        destroyed.sort();
        assert_eq!(destroyed, vec![a, b]);

        // A second cleanup is a no-op (the tracked set was drained).
        sandboxes.cleanup_all().await;
        assert_eq!(backend.destroyed.lock().await.len(), 2);
    }
}
