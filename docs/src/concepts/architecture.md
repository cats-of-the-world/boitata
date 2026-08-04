# Architecture

Boitata is a Rust workspace of small crates. Each crate has one job, and the
dependencies point inward toward `boitata-core`.

## Workspace layout

```
crates/
  boitata-core/          # Config, providers, tools, runtime, audit
  boitata-agent/         # The agent loop (LLM + tools) + the ACP agent server
  boitata-acp/           # Agent Client Protocol: serve an agent / drive one
  boitata-orchestrator/  # Blueprint graphs over the agent (+ sandbox nodes)
  boitata-cli/           # The `boitata` binary
  boitata-server/        # HTTP/SSE server + embedded web UI
```

## Dependency flow

```
 ┌─────────────┐   ┌──────────────┐
 │  CLI / Web  │   │  HTTP Server │        ← interfaces
 └──────┬──────┘   └──────┬───────┘
        │                 │
        └────────┬────────┘
                 │
        ┌────────▼─────────┐
        │   Orchestrator   │             ← blueprints
        │   (blueprints)   │
        └────────┬─────────┘
                 │
        ┌────────▼─────────┐
        │      Agent       │             ← agent loop
        │  (LLM + tools)   │
        └────────┬─────────┘
                 │
 ┌───────────────┼───────────────┐
 │               │               │
 ▼               ▼               ▼
Providers      Tools            Audit        ← core
(Anthropic,    (file, cargo,    (JSONL
 OpenAI,        git, search,    event log)
 Ollama)        MCP …)
```

## The crates

### `boitata-core`

The foundation. Holds everything the other crates build on:

- **Config** — TOML loading, env-var overrides, and manual `Debug` impls that
  redact secrets (`api_key`, MCP `auth_token`).
- **Providers** — the `Provider` trait and the Anthropic, OpenAI, and Ollama
  implementations.
- **Tools** — the `ToolRegistry`, the built-in tools, and the `ToolPolicy` that
  gates every call.
- **Runtime** — the shared assembly (`boitata_core::runtime`) that builds the
  provider, tools, and policy once so the CLI and server share identical setup.
- **Audit** — the append-only JSONL event log.

### `boitata-agent`

The core execution engine. It accepts a task description, then iterates through
LLM calls and tool executions while maintaining a token-budgeted conversation
context, and returns a structured result.

### `boitata-agent`

Also ships the `boitata-agent` binary: the agent exposed as an **ACP server** over
TCP, for running the agent inside a sandbox. See
[Sandboxed Execution](./sandboxed-execution.md).

### `boitata-acp`

The [Agent Client Protocol](https://agentclientprotocol.com/) integration: a
`serve()` that runs an agent as an ACP server and a `run_prompt()` client the
orchestrator uses to drive an agent running elsewhere. Boitata's audit events ride
inside the protocol's message chunks, so the existing audit/SSE stream is unchanged.

### `boitata-orchestrator`

The blueprint system. Compiles a YAML blueprint into a typed graph of `agent`,
`tool`, `script`, `human`, and **sandbox** (`provision` / `checkout` / `exec` /
`agent_sandbox`) nodes, then executes it with parallel super-steps, in-memory
checkpointing, and retry. It talks to sandboxes through a `Sandbox` backend trait
(Docker today, Firecracker planned). See [Blueprints](../reference/blueprints.md)
and [Sandboxed Execution](./sandboxed-execution.md).

### `boitata-cli`

The `boitata` binary. Parses arguments, loads config via `boitata_core::runtime`,
and either runs a single task or a blueprint — locally or scheduled on a remote
server (`--remote`).

### `boitata-server`

An HTTP/SSE backend with an embedded web UI. Reuses `boitata_core::runtime` to
build the provider, tools, and policy once, then serves them to concurrent runs.
See [Server & Web UI](../interfaces/server.md).

## One assembly, many front-ends

Because the CLI and the server both build their runtime through
`boitata_core::runtime`, a task runs identically whether launched from the
terminal, from the web UI, or scheduled remotely by the CLI's `--remote` mode.
The agent loop, tools, policy, and audit log are the same in every case.
