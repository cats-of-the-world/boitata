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
- [x] YAML blueprint definitions (`--blueprint <name>` or a path to your own `.yaml`)
- [x] Starter blueprints (`default`, `fix_lint_errors`, `fix_test_failure`,
      `setup_devbox`, `human_approval`)
- [x] Parallel super-steps (fan-out / fan-in), in-memory checkpoint + retry,
      human-in-the-loop

## Sprint 5: Workspace Management

- [ ] Workspace manager with isolation
- [ ] Snapshot/restore for retries
- [ ] Task queue and executor

## Sprint 6: Interfaces

- [ ] Full CLI implementation
- [ ] Web UI for task monitoring
- [ ] Testing integration (Rust-first)
