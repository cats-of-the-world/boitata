# Sandboxed Execution

By default the agent runs in-process on the host: the tools it calls
(`file_write`, `execute_command`, `git_*`, and so on) act on the host
filesystem, confined only by the [workspace root](../reference/security.md).
That is ideal for a trusted, single-user setup.

Boitata is also growing a second execution model: run the task inside an
isolated sandbox, so the untrusted, tool-wielding agent can't touch the host.
This page explains how that works and how the pieces fit together.

## The target model

```
  CLI            Server (orchestrator)             Sandbox
 (thin  --schedule-->  --run blueprint-->      +----------------+
  client)                                      | repo + deps    |
        <-- stream events (SSE) -- <-- ACP -->  | boitata-agent  |
                                               | (ACP server)   |
                                               +----------------+
```

1. The CLI schedules a task on the server (`boitata run ... --remote <url>`). It
   does not run the agent itself.
2. The server orchestrates: it runs a blueprint that provisions a sandbox, gets
   the code into it, and delegates the actual work to an agent running inside
   the sandbox.
3. The agent runs in the sandbox, exposed as an
   [Agent Client Protocol](https://agentclientprotocol.com/) (ACP) server. The
   orchestrator drives it over ACP and streams its events back to the web UI and
   audit log, identically to a local run.

The agent is self-contained: it has its own tools and works on the sandbox's own
filesystem, so it never needs the host.

> Status: this is the target model. The container blueprint runs today locally
> via the CLI (`boitata run --blueprint examples/blueprints/containerized_task.yaml`)
> and on the server when it is started with a trusted blueprints directory
> (`boitata-server --blueprints-dir examples/blueprints`), which is how the
> server-orchestrated flow above is enabled. The server offers only those vetted
> blueprints by name, never an arbitrary path from a network request.
>
> Its image must carry the in-container `boitata-agent`. Build it with
> `docker build -f examples/boitata-agent-rust.Dockerfile -t boitata-agent-rust:latest .`

## The pieces

### Sandboxes (`Sandbox` trait)

`boitata-orchestrator` talks to sandboxes through a small backend trait:
`provision` / `exec` / `endpoint` / `destroy`. The current backend is Docker
(via `bollard`); a Firecracker microVM backend is planned behind the same trait
for VM-grade isolation. The backend connects lazily, so blueprints that use no
sandbox never require a daemon. The executor auto-destroys every sandbox a run
provisioned when the run ends, whether it succeeds, fails, or is cancelled.

### Container blueprint nodes

Four node types move a run off the host (see
[Blueprints](../reference/blueprints.md)):

- `provision`: create an ephemeral sandbox from an image. Its output is the
  sandbox id, referenced downstream as `{name}`.
- `checkout`: `git clone` a repo into the sandbox.
- `exec`: run a shell command inside the sandbox, routing on its exit code.
- `agent_sandbox`: run the agent inside the sandbox over ACP (see below).

### The ACP agent (`boitata-agent` + `boitata-acp`)

`boitata-agent` is a binary that exposes the agent as an ACP server over TCP. It
builds a provider and tools from local config (the same `boitata_core::runtime`
wiring the CLI and server use), then answers ACP `session/prompt` requests by
running the agent and streaming its events.

`boitata-acp` is the shared protocol crate: a `serve()` that runs the agent as
an ACP server, and a `run_prompt()` client the orchestrator uses. Boitata's own
audit events ride inside the protocol's message chunks, so the orchestrator's
audit log and web UI stream work unchanged.

### The `agent_sandbox` node

This is the node that ties it together. Given a provisioned sandbox, it:

1. launches the agent inside it (`boitata-agent --addr 0.0.0.0:<port>`);
2. resolves the sandbox's address and waits for the agent to accept connections;
3. drives it with the ACP client, teeing the agent's events into the run's audit
   stream;
4. reports the agent's final message as the node's output and routes on success.

```yaml
name: containerized_agent
entry: box
nodes:
  box:   { type: provision, image: "your-image-with-boitata-agent" }
  code:  { type: checkout, container: "{box}", repo: "{task}" }
  work:  { type: agent_sandbox, container: "{box}", prompt: "make the tests pass" }
edges:
  - { from: box, to: code }
  - { from: code, when: success, to: work }
  - { from: work, to: END }
```

The image must contain the `boitata-agent` binary and a config with provider
credentials. Building such an image is part of the upcoming Firecracker/rootfs
work.

## Isolation and security

The sandbox gives OS-level isolation for the work done inside it. Note the
current limits, tracked as follow-ups (see [Security](../reference/security.md)):

- The `provision` node does not yet apply resource limits or capability drops;
  vanilla containers share the host kernel. Firecracker or Kata microVMs are the
  planned answer for a strong trust boundary.
- The server has no authentication. Put it behind a trusted network or an
  authenticating proxy.
- Provider credentials inside a sandbox are the sandbox's; don't hand a tenant a
  shared key.

These are why the roadmap treats Firecracker isolation and multi-tenant auth as
first-class next steps.
