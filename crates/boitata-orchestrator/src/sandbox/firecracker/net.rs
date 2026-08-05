//! Host networking for a Firecracker microVM: a TAP device per VM plus the NAT
//! rules that let the guest reach the network (git clone, the LLM API, …).
//!
//! Each VM gets a slot `n` (0–255) that deterministically fixes its TAP name and
//! a `/30` point-to-point subnet — host side `172.16.<n>.1`, guest side
//! `172.16.<n>.2` — so the host TAP address is the guest's default gateway. The
//! guest configures its interface statically from the kernel `ip=` boot arg (see
//! [`kernel_ip_arg`]); no DHCP in the guest.
//!
//! The allocation and the `ip`/`iptables` argv are **pure** (unit-tested); the
//! functions that actually run them ([`setup`]/[`teardown`]) require
//! `CAP_NET_ADMIN` and are exercised on a KVM host, not in CI.

use std::collections::HashSet;

use anyhow::Context;

/// The `/16` all VM point-to-point links live in. Slot `n` uses `172.16.<n>.0/30`.
const SUBNET_PREFIX: &str = "172.16";
/// Point-to-point mask (`/30`: network, host, guest, broadcast).
const NETMASK: &str = "255.255.255.252";

/// A VM's network identity, derived purely from its slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmNet {
    pub slot: u8,
    /// TAP device name on the host (≤ 15 chars for `IFNAMSIZ`).
    pub tap: String,
    /// Host-side address on the TAP; the guest's default gateway.
    pub host_ip: String,
    /// Guest-side address.
    pub guest_ip: String,
    /// Guest NIC MAC (locally administered).
    pub guest_mac: String,
}

impl VmNet {
    /// Derive the (deterministic) network identity for `slot`.
    pub fn for_slot(slot: u8) -> Self {
        Self {
            slot,
            tap: format!("fc-tap-{slot}"),
            host_ip: format!("{SUBNET_PREFIX}.{slot}.1"),
            guest_ip: format!("{SUBNET_PREFIX}.{slot}.2"),
            // 06 = locally administered, unicast; last byte .2 mirrors the guest IP.
            guest_mac: format!("06:00:ac:10:{slot:02x}:02"),
        }
    }

    /// The kernel `ip=` boot argument that statically configures the guest NIC:
    /// `ip=<guest>::<gateway>:<mask>::eth0:off` (off = no autoconf/DHCP).
    pub fn kernel_ip_arg(&self) -> String {
        format!(
            "ip={guest}::{gw}:{mask}::eth0:off",
            guest = self.guest_ip,
            gw = self.host_ip,
            mask = NETMASK,
        )
    }
}

/// Hands out unique VM slots (0–255), recycling them on release so a long-lived
/// server doesn't exhaust the space. Not thread-safe on its own — the sandbox
/// wraps it in a mutex.
#[derive(Debug, Default)]
pub struct SlotAllocator {
    in_use: HashSet<u8>,
}

impl SlotAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve the lowest free slot, or `None` if all 256 are taken.
    pub fn alloc(&mut self) -> Option<u8> {
        let slot = (0..=u8::MAX).find(|n| !self.in_use.contains(n))?;
        self.in_use.insert(slot);
        Some(slot)
    }

    /// Return a slot to the pool.
    pub fn release(&mut self, slot: u8) {
        self.in_use.remove(&slot);
    }
}

// --- Pure argv builders --------------------------------------------------------
//
// Each returns the argv for one privileged command. Kept pure so the exact
// commands are asserted in tests without touching the host.

/// `ip tuntap add <tap> mode tap` — create the TAP device.
pub fn tap_add_argv(net: &VmNet) -> Vec<String> {
    vec![
        "ip", "tuntap", "add", &net.tap, "mode", "tap",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `ip addr add <host_ip>/30 dev <tap>` — put the gateway address on the TAP.
pub fn tap_addr_argv(net: &VmNet) -> Vec<String> {
    vec![
        "ip".into(),
        "addr".into(),
        "add".into(),
        format!("{}/30", net.host_ip),
        "dev".into(),
        net.tap.clone(),
    ]
}

/// `ip link set <tap> up` — bring the TAP up.
pub fn tap_up_argv(net: &VmNet) -> Vec<String> {
    vec!["ip", "link", "set", &net.tap, "up"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// `ip link del <tap>` — remove the TAP device on teardown.
pub fn tap_del_argv(net: &VmNet) -> Vec<String> {
    vec!["ip", "link", "del", &net.tap]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Forward rule (add or delete) letting the guest's TAP egress via `egress_iface`.
/// `op` is `-A` to add or `-D` to delete.
fn forward_argv(op: &str, net: &VmNet, egress_iface: &str) -> Vec<String> {
    vec![
        "iptables", op, "FORWARD", "-i", &net.tap, "-o", egress_iface, "-j", "ACCEPT",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Reverse forward rule for established/related traffic back to the guest.
fn forward_back_argv(op: &str, net: &VmNet, egress_iface: &str) -> Vec<String> {
    vec![
        "iptables",
        op,
        "FORWARD",
        "-i",
        egress_iface,
        "-o",
        &net.tap,
        "-m",
        "state",
        "--state",
        "RELATED,ESTABLISHED",
        "-j",
        "ACCEPT",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// MASQUERADE rule (add or delete) for a VM's `/30` out of `egress_iface`, so the
/// guest's traffic is NAT'd to the host address.
fn masquerade_argv(op: &str, net: &VmNet, egress_iface: &str) -> Vec<String> {
    vec![
        "iptables".into(),
        "-t".into(),
        "nat".into(),
        op.into(),
        "POSTROUTING".into(),
        "-s".into(),
        format!("{}.{}.0/30", SUBNET_PREFIX, net.slot),
        "-o".into(),
        egress_iface.into(),
        "-j".into(),
        "MASQUERADE".into(),
    ]
}

/// All commands to bring a VM's networking up, in order.
fn setup_argvs(net: &VmNet, egress_iface: &str) -> Vec<Vec<String>> {
    vec![
        tap_add_argv(net),
        tap_addr_argv(net),
        tap_up_argv(net),
        masquerade_argv("-A", net, egress_iface),
        forward_argv("-A", net, egress_iface),
        forward_back_argv("-A", net, egress_iface),
    ]
}

/// All commands to tear a VM's networking down. NAT/forward rules are deleted
/// before the TAP so nothing dangles; each is best-effort on teardown.
fn teardown_argvs(net: &VmNet, egress_iface: &str) -> Vec<Vec<String>> {
    vec![
        masquerade_argv("-D", net, egress_iface),
        forward_argv("-D", net, egress_iface),
        forward_back_argv("-D", net, egress_iface),
        tap_del_argv(net),
    ]
}

// --- Side-effecting wrappers (need CAP_NET_ADMIN; verified on a KVM host) -------

/// Create the TAP and install NAT/forward rules for `net`. Fails on the first
/// command that errors, after which the caller should [`teardown`] to undo any
/// partial state.
pub async fn setup(net: &VmNet, egress_iface: &str) -> anyhow::Result<()> {
    // Enable IP forwarding once (idempotent); ignore if the knob isn't writable
    // here — the FORWARD rules still need it, and a hard failure is surfaced by
    // the guest simply not reaching the network.
    let _ = run(&["sysctl", "-w", "net.ipv4.ip_forward=1"]).await;
    for argv in setup_argvs(net, egress_iface) {
        run_owned(argv).await?;
    }
    Ok(())
}

/// Best-effort teardown: run every removal command, logging (not failing) on
/// error so one dangling rule can't block cleanup of the rest.
pub async fn teardown(net: &VmNet, egress_iface: &str) {
    for argv in teardown_argvs(net, egress_iface) {
        if let Err(e) = run_owned(argv.clone()).await {
            tracing::warn!("firecracker net teardown `{}` failed: {e:#}", argv.join(" "));
        }
    }
}

async fn run(argv: &[&str]) -> anyhow::Result<()> {
    run_owned(argv.iter().map(|s| s.to_string()).collect()).await
}

/// Run a privileged command, treating a non-zero exit as an error (with the
/// command's stderr for context).
async fn run_owned(argv: Vec<String>) -> anyhow::Result<()> {
    let (cmd, args) = argv.split_first().context("empty command")?;
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run `{}`", argv.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "`{}` failed ({}): {}",
            argv.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmnet_derives_deterministic_addresses() {
        let net = VmNet::for_slot(7);
        assert_eq!(net.tap, "fc-tap-7");
        assert_eq!(net.host_ip, "172.16.7.1");
        assert_eq!(net.guest_ip, "172.16.7.2");
        assert_eq!(net.guest_mac, "06:00:ac:10:07:02");
    }

    #[test]
    fn kernel_ip_arg_is_static_no_dhcp() {
        let net = VmNet::for_slot(3);
        assert_eq!(
            net.kernel_ip_arg(),
            "ip=172.16.3.2::172.16.3.1:255.255.255.252::eth0:off"
        );
    }

    #[test]
    fn allocator_hands_out_unique_slots_and_recycles() {
        let mut a = SlotAllocator::new();
        let s0 = a.alloc().unwrap();
        let s1 = a.alloc().unwrap();
        assert_eq!((s0, s1), (0, 1));
        a.release(s0);
        // The freed slot is reused ahead of a fresh one.
        assert_eq!(a.alloc().unwrap(), 0);
        assert_eq!(a.alloc().unwrap(), 2);
    }

    #[test]
    fn tap_argvs_are_exact() {
        let net = VmNet::for_slot(5);
        assert_eq!(tap_add_argv(&net), ["ip", "tuntap", "add", "fc-tap-5", "mode", "tap"]);
        assert_eq!(
            tap_addr_argv(&net),
            ["ip", "addr", "add", "172.16.5.1/30", "dev", "fc-tap-5"]
        );
        assert_eq!(tap_up_argv(&net), ["ip", "link", "set", "fc-tap-5", "up"]);
        assert_eq!(tap_del_argv(&net), ["ip", "link", "del", "fc-tap-5"]);
    }

    #[test]
    fn nat_rules_scope_to_the_vm_subnet() {
        let net = VmNet::for_slot(9);
        assert_eq!(
            masquerade_argv("-A", &net, "eth0"),
            [
                "iptables", "-t", "nat", "-A", "POSTROUTING", "-s", "172.16.9.0/30", "-o", "eth0",
                "-j", "MASQUERADE"
            ]
        );
        // Teardown mirrors setup with -D, and removes the tap last.
        let down = teardown_argvs(&net, "eth0");
        assert_eq!(down.len(), 4);
        assert_eq!(down[0][3], "-D");
        assert_eq!(down.last().unwrap(), &tap_del_argv(&net));
    }
}
