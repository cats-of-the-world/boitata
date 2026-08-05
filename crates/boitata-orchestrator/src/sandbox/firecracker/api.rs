//! The Firecracker configuration API: build the request bodies and speak the
//! minimal HTTP the daemon expects over its control Unix socket.
//!
//! Firecracker is configured by a sequence of `PUT`s to its API socket
//! (`--api-sock`), then an `InstanceStart` action boots the VM. The bodies are
//! small JSON documents built purely here (unit-tested); [`put`] is a tiny
//! HTTP/1.1-over-UDS client so we don't pull in a full HTTP stack for a handful of
//! one-shot requests. Actually talking to a live daemon is exercised on a KVM
//! host, not in CI.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::net::VmNet;

/// One configuration request: the API path and its JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub path: String,
    pub body: Value,
}

/// `PUT /boot-source` — the kernel and its boot arguments. The `ip=` arg (from
/// [`VmNet::kernel_ip_arg`]) statically configures the guest NIC; the console/
/// panic args are the Firecracker-recommended minimal set.
pub fn boot_source(kernel_path: &str, net: &VmNet) -> Request {
    let boot_args = format!(
        "console=ttyS0 reboot=k panic=1 pci=off {}",
        net.kernel_ip_arg()
    );
    Request {
        path: "/boot-source".into(),
        body: json!({ "kernel_image_path": kernel_path, "boot_args": boot_args }),
    }
}

/// `PUT /drives/rootfs` — the root filesystem drive (writable per-VM overlay).
pub fn root_drive(rootfs_path: &str) -> Request {
    Request {
        path: "/drives/rootfs".into(),
        body: json!({
            "drive_id": "rootfs",
            "path_on_host": rootfs_path,
            "is_root_device": true,
            "is_read_only": false,
        }),
    }
}

/// `PUT /network-interfaces/eth0` — attach the host TAP as the guest's eth0.
pub fn network_interface(net: &VmNet) -> Request {
    Request {
        path: "/network-interfaces/eth0".into(),
        body: json!({
            "iface_id": "eth0",
            "guest_mac": net.guest_mac,
            "host_dev_name": net.tap,
        }),
    }
}

/// `PUT /machine-config` — vCPU and memory sizing.
pub fn machine_config(vcpus: u32, mem_mib: u32) -> Request {
    Request {
        path: "/machine-config".into(),
        body: json!({ "vcpu_count": vcpus, "mem_size_mib": mem_mib }),
    }
}

/// `PUT /mmds/config` — expose the metadata service on eth0 so the guest can read
/// it at the link-local `169.254.169.254`.
pub fn mmds_config() -> Request {
    Request {
        path: "/mmds/config".into(),
        body: json!({ "network_interfaces": ["eth0"] }),
    }
}

/// `PUT /mmds` — the metadata itself: the ephemeral SSH public key the guest must
/// trust and the environment to export. These may be **secrets**; they travel
/// only over the local API socket and MMDS, never a log or the kernel cmdline.
pub fn mmds_data(ssh_authorized_key: &str, env: &[(String, String)]) -> Request {
    let env_map: serde_json::Map<String, Value> = env
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    Request {
        path: "/mmds".into(),
        body: json!({
            "ssh": { "authorized_key": ssh_authorized_key },
            "env": env_map,
        }),
    }
}

/// `PUT /actions {InstanceStart}` — boot the configured VM.
pub fn instance_start() -> Request {
    Request {
        path: "/actions".into(),
        body: json!({ "action_type": "InstanceStart" }),
    }
}

/// The ordered configuration requests to fully set up (but not yet start) a VM.
pub fn configure_requests(
    kernel_path: &str,
    rootfs_path: &str,
    net: &VmNet,
    vcpus: u32,
    mem_mib: u32,
    ssh_authorized_key: &str,
    env: &[(String, String)],
) -> Vec<Request> {
    vec![
        machine_config(vcpus, mem_mib),
        boot_source(kernel_path, net),
        root_drive(rootfs_path),
        network_interface(net),
        mmds_config(),
        mmds_data(ssh_authorized_key, env),
    ]
}

/// Serialize a `PUT` request to the raw HTTP/1.1 bytes Firecracker's API expects:
/// request line, `Host`, JSON content type + length, `Connection: close`, then the
/// body. Pure, so the wire format is unit-testable.
pub fn encode_put(path: &str, body: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(body).expect("serde_json::Value always serializes");
    let mut out = Vec::with_capacity(body.len() + 128);
    let header = format!(
        "PUT {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Accept: application/json\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&body);
    out
}

/// Send one `PUT` to the API socket and require a 2xx response. Opens a fresh
/// connection per request (`Connection: close`), which the API server accepts.
pub async fn put(socket: &Path, req: &Request) -> anyhow::Result<()> {
    let bytes = encode_put(&req.path, &req.body);
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("failed to connect to firecracker api socket {socket:?}"))?;
    stream
        .write_all(&bytes)
        .await
        .context("failed to write firecracker api request")?;
    stream.flush().await.ok();

    let mut resp = Vec::new();
    // Bound the read so a wedged daemon can't hang us; the responses are tiny.
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut resp))
        .await
        .with_context(|| format!("timed out reading response for PUT {}", req.path))?
        .context("failed to read firecracker api response")?;

    let status = parse_status_code(&resp)
        .with_context(|| format!("unparseable firecracker response for PUT {}", req.path))?;
    if !(200..300).contains(&status) {
        bail!(
            "firecracker PUT {} returned {status}: {}",
            req.path,
            response_body(&resp)
        );
    }
    Ok(())
}

/// Parse the numeric status code from an HTTP/1.1 status line (`HTTP/1.1 204 …`).
fn parse_status_code(resp: &[u8]) -> anyhow::Result<u16> {
    let text = String::from_utf8_lossy(resp);
    let line = text.lines().next().context("empty response")?;
    let code = line
        .split_whitespace()
        .nth(1)
        .context("missing status code")?;
    code.parse::<u16>().context("non-numeric status code")
}

/// The response body (after the header/body separator), for error context.
fn response_body(resp: &[u8]) -> String {
    let text = String::from_utf8_lossy(resp);
    match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.trim().to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_source_carries_static_ip_and_panic_args() {
        let net = VmNet::for_slot(4);
        let req = boot_source("/img/vmlinux", &net);
        assert_eq!(req.path, "/boot-source");
        assert_eq!(req.body["kernel_image_path"], "/img/vmlinux");
        let args = req.body["boot_args"].as_str().unwrap();
        assert!(args.contains("panic=1"));
        assert!(args.contains("ip=172.16.4.2::172.16.4.1:255.255.255.252::eth0:off"));
    }

    #[test]
    fn root_drive_is_writable_root() {
        let req = root_drive("/vm/rootfs.ext4");
        assert_eq!(req.body["is_root_device"], true);
        assert_eq!(req.body["is_read_only"], false);
        assert_eq!(req.body["path_on_host"], "/vm/rootfs.ext4");
    }

    #[test]
    fn network_interface_binds_tap_and_mac() {
        let net = VmNet::for_slot(2);
        let req = network_interface(&net);
        assert_eq!(req.body["host_dev_name"], "fc-tap-2");
        assert_eq!(req.body["guest_mac"], "06:00:ac:10:02:02");
    }

    #[test]
    fn mmds_data_carries_key_and_env() {
        let req = mmds_data("ssh-ed25519 AAAA...", &[("BOITATA_API_KEY".into(), "secret".into())]);
        assert_eq!(req.body["ssh"]["authorized_key"], "ssh-ed25519 AAAA...");
        assert_eq!(req.body["env"]["BOITATA_API_KEY"], "secret");
    }

    #[test]
    fn configure_requests_is_ordered_machine_first() {
        let net = VmNet::for_slot(1);
        let reqs = configure_requests("/k", "/r", &net, 2, 1024, "key", &[]);
        let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/machine-config",
                "/boot-source",
                "/drives/rootfs",
                "/network-interfaces/eth0",
                "/mmds/config",
                "/mmds",
            ]
        );
    }

    #[test]
    fn encode_put_frames_headers_and_body() {
        let bytes = encode_put("/machine-config", &json!({ "vcpu_count": 2 }));
        let text = String::from_utf8(bytes).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("PUT /machine-config HTTP/1.1\r\n"));
        assert!(head.contains("Content-Type: application/json"));
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        assert_eq!(body, r#"{"vcpu_count":2}"#);
    }

    #[test]
    fn parses_status_codes() {
        assert_eq!(
            parse_status_code(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap(),
            204
        );
        let err = b"HTTP/1.1 400 Bad Request\r\n\r\n{\"fault_message\":\"nope\"}";
        assert_eq!(parse_status_code(err).unwrap(), 400);
        assert_eq!(response_body(err), r#"{"fault_message":"nope"}"#);
    }
}
