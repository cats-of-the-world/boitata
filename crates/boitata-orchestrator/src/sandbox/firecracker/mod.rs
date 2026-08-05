//! The Firecracker backend for [`Sandbox`](super::Sandbox): each sandbox is an
//! ephemeral microVM for VM-grade isolation (its own kernel), unlike the Docker
//! backend's shared-kernel containers.
//!
//! A `provision` boots a VM from a configured kernel + a per-VM copy of the base
//! rootfs, on a private `/30` TAP link (see [`net`]); `exec` runs commands in the
//! guest over SSH (see [`ssh`]); `endpoint` returns the guest's `ip:port` so the
//! in-VM `boitata-agent` ACP server is reached over plain TCP; `destroy` kills the
//! VM and tears its networking down. The ephemeral SSH key and the forwarded env
//! reach the guest via Firecracker's metadata service (MMDS), never a log or the
//! kernel cmdline.
//!
//! Host requirements: `/dev/kvm`, `CAP_NET_ADMIN` (for the TAP + NAT), and a
//! `firecracker` binary. Booting is therefore verified on a KVM host; the pure
//! request/argv/allocation logic in the submodules is unit-tested everywhere.

mod api;
mod net;
mod ssh;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use super::Sandbox;
use boitata_core::config::FirecrackerConfig;
use net::{SlotAllocator, VmNet};

/// How long to wait for the API socket to appear after launching firecracker.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for the guest to accept SSH after boot.
const SSH_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A running microVM and the host resources backing it, all released on
/// [`FirecrackerSandbox::destroy`].
struct Vm {
    slot: u8,
    net: VmNet,
    /// The firecracker process (`kill_on_drop`, so a dropped handle also stops it).
    child: Child,
    /// Per-VM working directory (API socket, rootfs overlay, keypair).
    workdir: PathBuf,
    /// Private key path used to SSH into the guest.
    key_path: PathBuf,
}

/// A [`Sandbox`] backed by Firecracker microVMs.
pub struct FirecrackerSandbox {
    config: FirecrackerConfig,
    slots: Mutex<SlotAllocator>,
    vms: Mutex<HashMap<String, Vm>>,
}

impl FirecrackerSandbox {
    pub fn new(config: FirecrackerConfig) -> Self {
        Self {
            config,
            slots: Mutex::new(SlotAllocator::new()),
            vms: Mutex::new(HashMap::new()),
        }
    }

    /// Base directory for per-VM working files.
    fn work_base(&self) -> PathBuf {
        self.config
            .work_dir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Look up a VM's SSH target `(key_path, user, guest_ip)` without holding the
    /// registry lock across an `.await`.
    fn ssh_target(&self, id: &str) -> anyhow::Result<(String, String, String)> {
        let vms = self.vms.lock().unwrap();
        let vm = vms
            .get(id)
            .with_context(|| format!("no such firecracker sandbox `{id}`"))?;
        Ok((
            vm.key_path.to_string_lossy().into_owned(),
            self.config.ssh_user.clone(),
            vm.net.guest_ip.clone(),
        ))
    }
}

#[async_trait]
impl Sandbox for FirecrackerSandbox {
    async fn provision(
        &self,
        _image: &str,
        env: &[(String, String)],
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        // `image` is ignored: the kernel + rootfs come from config. (A future
        // multi-image setup could select on it.)
        let slot = self
            .slots
            .lock()
            .unwrap()
            .alloc()
            .context("no free firecracker VM slots (256 in use)")?;
        let net = VmNet::for_slot(slot);
        let id = format!("fc-{}", uuid::Uuid::new_v4().simple());

        // Build the VM, tearing down every partial resource on any failure so a
        // botched provision never leaks a TAP, a process, or a directory.
        let mut partial = Partial::new(slot, net.clone(), self.work_base().join(&id));
        match self.boot(&id, &net, env, &mut partial, cancel).await {
            Ok(vm) => {
                self.vms.lock().unwrap().insert(id.clone(), vm);
                Ok(id)
            }
            Err(e) => {
                partial.cleanup(&self.config.egress_iface).await;
                self.slots.lock().unwrap().release(slot);
                Err(e)
            }
        }
    }

    async fn exec(
        &self,
        id: &str,
        argv: Vec<String>,
        workdir: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(i64, String)> {
        let (key, user, host) = self.ssh_target(id)?;
        ssh::exec(&key, &user, &host, &argv, workdir, cancel).await
    }

    async fn endpoint(&self, id: &str, port: u16) -> anyhow::Result<String> {
        let vms = self.vms.lock().unwrap();
        let vm = vms
            .get(id)
            .with_context(|| format!("no such firecracker sandbox `{id}`"))?;
        Ok(format!("{}:{port}", vm.net.guest_ip))
    }

    async fn destroy(&self, id: &str) -> anyhow::Result<()> {
        let vm = self.vms.lock().unwrap().remove(id);
        let Some(mut vm) = vm else {
            return Ok(()); // already gone; idempotent
        };
        // Stop the VM, then undo its networking and files. Best-effort throughout.
        let _ = vm.child.start_kill();
        let _ = vm.child.wait().await;
        net::teardown(&vm.net, &self.config.egress_iface).await;
        if let Err(e) = std::fs::remove_dir_all(&vm.workdir) {
            tracing::warn!("failed to remove firecracker workdir {:?}: {e}", vm.workdir);
        }
        self.slots.lock().unwrap().release(vm.slot);
        Ok(())
    }
}

impl FirecrackerSandbox {
    /// Provision a VM end-to-end: prepare files, set up networking, launch and
    /// configure firecracker, boot, and wait for SSH. Records each resource it
    /// creates on `partial` so the caller can tear down on failure.
    async fn boot(
        &self,
        id: &str,
        net: &VmNet,
        env: &[(String, String)],
        partial: &mut Partial,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vm> {
        std::fs::create_dir_all(&partial.workdir)
            .with_context(|| format!("failed to create workdir {:?}", partial.workdir))?;

        // Per-VM writable rootfs (copy the base so runs never mutate it) and an
        // ephemeral SSH keypair whose public half the guest will trust via MMDS.
        let rootfs = partial.workdir.join("rootfs.ext4");
        std::fs::copy(&self.config.rootfs, &rootfs)
            .with_context(|| format!("failed to copy rootfs from {}", self.config.rootfs))?;
        let key_path = partial.workdir.join("id_ed25519");
        let authorized_key = generate_ssh_key(&key_path).await?;

        // Host networking (needs CAP_NET_ADMIN).
        net::setup(net, &self.config.egress_iface).await?;
        partial.net_up = true;

        // Launch firecracker with its API socket, then configure and start the VM.
        let socket = partial.workdir.join("firecracker.sock");
        let child = Command::new(&self.config.firecracker_bin)
            .arg("--api-sock")
            .arg(&socket)
            .arg("--id")
            .arg(id)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to launch `{}`", self.config.firecracker_bin))?;
        partial.child = Some(child);

        wait_for_socket(&socket, SOCKET_TIMEOUT, cancel).await?;

        let rootfs_str = rootfs.to_string_lossy();
        let requests = api::configure_requests(
            &self.config.kernel,
            &rootfs_str,
            net,
            self.config.vcpus,
            self.config.mem_mib,
            &authorized_key,
            env,
        );
        for req in &requests {
            api::put(&socket, req).await?;
        }
        api::put(&socket, &api::instance_start()).await?;

        // Wait for sshd inside the guest before handing the VM back.
        ssh::wait_ready(
            &key_path.to_string_lossy(),
            &self.config.ssh_user,
            &net.guest_ip,
            SSH_READY_TIMEOUT,
            cancel,
        )
        .await?;

        Ok(Vm {
            slot: partial.slot,
            net: net.clone(),
            child: partial.child.take().expect("child was set above"),
            workdir: std::mem::take(&mut partial.workdir),
            key_path,
        })
    }
}

/// Tracks the resources a `provision` has created so far, so a failure partway
/// through can undo exactly what was done.
struct Partial {
    slot: u8,
    net: VmNet,
    workdir: PathBuf,
    child: Option<Child>,
    net_up: bool,
}

impl Partial {
    fn new(slot: u8, net: VmNet, workdir: PathBuf) -> Self {
        Self {
            slot,
            net,
            workdir,
            child: None,
            net_up: false,
        }
    }

    /// Best-effort teardown of everything provisioned before the failure.
    async fn cleanup(&mut self, egress_iface: &str) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if self.net_up {
            net::teardown(&self.net, egress_iface).await;
        }
        if self.workdir.as_os_str().is_empty() {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.workdir) {
            // Missing dir is fine (we may have failed before creating it).
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to clean firecracker workdir {:?}: {e}", self.workdir);
            }
        }
    }
}

/// Generate an ephemeral ed25519 keypair at `key_path` and return the public key
/// line (to be trusted by the guest). Uses the system `ssh-keygen`.
async fn generate_ssh_key(key_path: &Path) -> anyhow::Result<String> {
    let status = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(key_path)
        .arg("-q")
        .status()
        .await
        .context("failed to run ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed ({status})");
    }
    let pub_path = key_path.with_extension("pub");
    // ssh-keygen appends `.pub`; `with_extension` on a path with no extension
    // yields exactly that.
    let key = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("failed to read generated public key {pub_path:?}"))?;
    Ok(key.trim().to_string())
}

/// Poll until the firecracker API socket exists (it's created shortly after
/// launch) or `timeout` elapses. Cancellable.
async fn wait_for_socket(
    socket: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled while waiting for the firecracker api socket");
        }
        if socket.exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("firecracker api socket {socket:?} did not appear within {timeout:?}");
        }
        tokio::select! {
            _ = cancel.cancelled() => bail!("cancelled while waiting for the firecracker api socket"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FirecrackerConfig {
        FirecrackerConfig {
            kernel: "/img/vmlinux".into(),
            rootfs: "/img/rootfs.ext4".into(),
            ssh_user: "root".into(),
            vcpus: 2,
            mem_mib: 1024,
            egress_iface: "eth0".into(),
            firecracker_bin: "firecracker".into(),
            work_dir: Some("/tmp/fc".into()),
        }
    }

    #[test]
    fn work_base_uses_configured_dir() {
        let sb = FirecrackerSandbox::new(config());
        assert_eq!(sb.work_base(), PathBuf::from("/tmp/fc"));
    }

    #[tokio::test]
    async fn endpoint_errors_for_unknown_sandbox() {
        let sb = FirecrackerSandbox::new(config());
        assert!(sb.endpoint("nope", 9000).await.is_err());
    }

    #[tokio::test]
    async fn destroy_unknown_sandbox_is_ok() {
        // Idempotent: destroying an unknown/already-gone VM is not an error.
        let sb = FirecrackerSandbox::new(config());
        assert!(sb.destroy("nope").await.is_ok());
    }
}
