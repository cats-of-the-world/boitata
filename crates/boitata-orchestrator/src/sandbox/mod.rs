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
mod firecracker;

use std::sync::Arc;

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use boitata_core::config::Config;
use tokio_util::sync::CancellationToken;

/// Give up on a single sandbox teardown that hangs this long, so an unresponsive
/// daemon can't block the orchestrator's shutdown forever.
const DESTROY_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on captured exec output, shared across backends. A verbose command
/// shouldn't be able to OOM the orchestrator; output past this is dropped with a
/// trailing marker.
pub(crate) const MAX_EXEC_OUTPUT: usize = 1 << 20; // 1 MiB

pub use docker::DockerSandbox;
pub use firecracker::FirecrackerSandbox;

/// Select a sandbox backend from config. Returns `None` for the default
/// (`docker`) — the executor already falls back to Docker — and `Some(backend)`
/// to override it (`firecracker`). A caller wires the result via
/// [`Executor::with_sandbox`](crate::Executor::with_sandbox).
///
/// Errors if `sandbox = "firecracker"` without a `[firecracker]` section, or on
/// an unknown backend name.
pub fn build_sandbox(config: &Config) -> anyhow::Result<Option<Arc<dyn Sandbox>>> {
    match config.sandbox.as_deref() {
        None | Some("docker") => Ok(None),
        Some("firecracker") => {
            let fc = config.firecracker.clone().context(
                "sandbox = \"firecracker\" requires a [firecracker] config section \
                 (kernel, rootfs, egress_iface)",
            )?;
            Ok(Some(Arc::new(FirecrackerSandbox::new(fc))))
        }
        Some(other) => bail!(
            "unknown sandbox backend `{other}` (expected `docker` or `firecracker`)"
        ),
    }
}

/// Append `chunk` to `output` without exceeding [`MAX_EXEC_OUTPUT`], truncating
/// the appended slice to the remaining budget on a UTF-8 char boundary so a
/// single large chunk can't overshoot the cap. Sets `truncated` when it clips.
pub(crate) fn append_capped(output: &mut String, chunk: &str, truncated: &mut bool) {
    let remaining = MAX_EXEC_OUTPUT.saturating_sub(output.len());
    if remaining == 0 {
        *truncated = !chunk.is_empty();
        return;
    }
    if chunk.len() <= remaining {
        output.push_str(chunk);
    } else {
        let mut end = remaining;
        while end > 0 && !chunk.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&chunk[..end]);
        *truncated = true;
    }
}

/// A backend that can create isolated environments, run commands in them, and
/// destroy them. `image` is an opaque "what to boot" spec the backend
/// interprets (Docker: an image name); it may widen to a struct for backends
/// (e.g. Firecracker) that need a kernel + rootfs.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Create and start an environment, returning an opaque id. Must not leave a
    /// half-provisioned environment behind on failure.
    ///
    /// `env` is a set of already-resolved `(name, value)` environment variables to
    /// inject into the environment. These values may be **secrets** (e.g. an API
    /// key the in-sandbox agent needs), so an implementation must pass them only to
    /// the sandbox's own environment — never log them, echo them onto a command
    /// line, or include them in any error.
    async fn provision(
        &self,
        image: &str,
        env: &[(String, String)],
        cancel: &CancellationToken,
    ) -> anyhow::Result<String>;

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

    /// Resolve a `host:port` the host can connect to for a service listening on
    /// `port` inside sandbox `id` (Docker: the container's bridge IP).
    async fn endpoint(&self, id: &str, port: u16) -> anyhow::Result<String>;

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
    /// no orphan on its own failure, so only successfully-provisioned ids are
    /// tracked. The tracking push is synchronous (a `std::sync::Mutex`), so
    /// nothing yields between obtaining the id and recording it. Cancellation in
    /// this codebase is cooperative (the provision future runs to completion
    /// rather than being force-dropped mid-call), so the id is always recorded;
    /// a hard task-abort mid-`provision` is out of scope and would be handled by
    /// label-based orphan reaping, not client-side tracking.
    ///
    /// `env` carries already-resolved `(name, value)` pairs to inject; values may
    /// be secrets and are forwarded straight to the backend, never logged here.
    pub async fn provision(
        &self,
        image: &str,
        env: &[(String, String)],
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let id = self.backend.provision(image, env, cancel).await?;
        self.provisioned.lock().unwrap().push(id.clone());
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

    pub async fn endpoint(&self, id: &str, port: u16) -> anyhow::Result<String> {
        self.backend.endpoint(id, port).await
    }

    /// Destroy every provisioned environment, concurrently. Best-effort: failures
    /// are logged, never propagated, so cleanup can't mask the run's own outcome.
    pub async fn cleanup_all(&self) {
        let ids = std::mem::take(&mut *self.provisioned.lock().unwrap());
        let backend = &self.backend;
        futures::future::join_all(ids.into_iter().map(|id| async move {
            // Bound each teardown so a hung daemon can't stall shutdown forever.
            match tokio::time::timeout(DESTROY_TIMEOUT, backend.destroy(&id)).await {
                Ok(Ok(())) => tracing::info!("destroyed sandbox {id}"),
                Ok(Err(e)) => tracing::warn!("failed to destroy sandbox {id}: {e}"),
                Err(_) => tracing::warn!("timed out destroying sandbox {id}"),
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
        async fn provision(
            &self,
            image: &str,
            _env: &[(String, String)],
            _c: &CancellationToken,
        ) -> anyhow::Result<String> {
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
        async fn endpoint(&self, id: &str, port: u16) -> anyhow::Result<String> {
            Ok(format!("{id}:{port}"))
        }
        async fn destroy(&self, id: &str) -> anyhow::Result<()> {
            self.destroyed.lock().unwrap().push(id.to_string());
            Ok(())
        }
    }

    #[test]
    fn append_capped_bounds_output_on_char_boundary() {
        let mut out = String::new();
        let mut truncated = false;
        // A chunk far larger than the cap is clipped to exactly the budget.
        let big = "x".repeat(MAX_EXEC_OUTPUT + 100);
        append_capped(&mut out, &big, &mut truncated);
        assert_eq!(out.len(), MAX_EXEC_OUTPUT);
        assert!(truncated);

        // Once full, further chunks are dropped, not appended.
        append_capped(&mut out, "more", &mut truncated);
        assert_eq!(out.len(), MAX_EXEC_OUTPUT);
    }

    #[test]
    fn append_capped_respects_utf8_boundaries() {
        // Budget lands mid multi-byte char; the append backs off to a boundary.
        let mut out = "a".repeat(MAX_EXEC_OUTPUT - 1);
        let mut truncated = false;
        append_capped(&mut out, "€", &mut truncated); // 3 bytes, only 1 byte left
        assert_eq!(out.len(), MAX_EXEC_OUTPUT - 1); // nothing partial appended
        assert!(truncated);
        assert!(out.is_char_boundary(out.len()));
    }

    #[tokio::test]
    async fn cleanup_destroys_every_provisioned_sandbox() {
        let backend = Arc::new(FakeSandbox::default());
        let sandboxes = Sandboxes::new(backend.clone());
        let cancel = CancellationToken::new();

        let a = sandboxes.provision("img", &[], &cancel).await.unwrap();
        let b = sandboxes.provision("img", &[], &cancel).await.unwrap();
        sandboxes.cleanup_all().await;

        let mut destroyed = backend.destroyed.lock().unwrap().clone();
        destroyed.sort();
        assert_eq!(destroyed, vec![a, b]);

        // A second cleanup is a no-op (the tracked set was drained).
        sandboxes.cleanup_all().await;
        assert_eq!(backend.destroyed.lock().unwrap().len(), 2);
    }
}
