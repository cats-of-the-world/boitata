# Roadmap

Where Boitata is headed. Checked items are done.

## Sprint 1: Foundation

- [x] Provider trait with multi-provider support
- [x] Agent loop with context management
- [x] Tool registry and first built-in tools
- [x] File system tools (read, write, list)

## Sprint 2: Tools

- [x] Code operations (cargo check, clippy, fmt, test, add)
- [x] Search tools (ripgrep integration)
- [x] Git operations (status, diff, commit, branch)
- [x] Command execution with safety checks (timeout, output cap, opt-out)

## Sprint 3: MCP Integration

- [x] MCP client implementation (via `rmcp`)
- [x] Tool discovery and registration
- [x] Remote transport (Streamable HTTP) + stdio
- [x] Resource access for context gathering

## Sprint 4: Blueprint System

- [x] Hybrid deterministic/agentic workflows (agent/tool/script/human node graphs)
- [x] YAML blueprint definitions (`--blueprint <path>` to a user-provided `.yaml`)
- [x] Example blueprints under `examples/blueprints/` (`default`, `fix_lint_errors`,
      `fix_test_failure`, `setup_devbox`, `human_approval`, `containerized_task`)
- [x] Parallel super-steps (fan-out / fan-in), in-memory checkpoint + retry,
      human-in-the-loop

## Sprint 5: Interfaces

- [x] Cargo workspace of focused crates (core / agent / orchestrator / cli / server)
- [x] HTTP/SSE server with an embedded React web UI for task monitoring
- [x] CLI `--remote` mode — schedule a task on a server and stream its progress

## Sprint 6: Sandboxed Execution

- [x] Container blueprint nodes (`provision` / `checkout` / `exec`) via Docker,
      with automatic teardown at run end
- [x] `Sandbox` backend trait so other backends can slot in
- [x] `boitata-agent` as an Agent Client Protocol (ACP) server + client
      (`boitata-acp`)
- [x] `agent_sandbox` node — run the agent **inside** a sandbox over ACP
- [x] Firecracker microVM `Sandbox` backend — boots a VM per sandbox, runs
      commands over SSH on a private TAP link, and reaches the in-VM agent over
      TCP/ACP (`sandbox = "firecracker"`); needs `/dev/kvm` + `CAP_NET_ADMIN`
- [ ] A rootfs image recipe (sshd + injected-key boot hook + toolchain +
      `boitata-agent`) and a pinned guest kernel
- [ ] End-to-end: a task spins up a VM with the code, its deps, and an agent that
      edits / compiles / tests inside it

## Later

- [ ] Container/VM hardening (resource limits, dropped capabilities, network policy)
- [ ] Multi-tenant authentication + per-run credential isolation
- [ ] Snapshot/restore for retries; a durable task queue
