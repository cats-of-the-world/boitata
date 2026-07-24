# LLM Usage

This repo was made mainly using LLMs.

# Boitata

> A fire serpent from Brazilian folklore that protects the forest and brings light to darkness.

**Boitata** is a one-shot, end-to-end coding agent inspired by Stripe's [Minions](https://stripe.dev/blog/minions-stripes-one-shot-end-to-end-coding-agents-part-1) and Block's [Goose](https://github.com/block/goose). Built in Rust, it enables unattended task execution with minimal human intervention.

## Philosophy: Determinism First

A core principle of Boitata is to **use deterministic tools whenever possible** instead of asking the LLM to do everything. This approach:

- **Reduces token costs** - Tools run without LLM involvement
- **Improves reliability** - Deterministic operations have predictable outcomes
- **Enables faster iteration** - Quick feedback loops without API calls
- **Maintains consistency** - Same inputs always produce the same outputs

### Examples

| Task | LLM Approach | Deterministic Approach |
|------|--------------|----------------------|
| Fix lint errors | "Read file, identify errors, fix them manually" | `cargo clippy --fix` |
| Format code | "Identify formatting issues and apply fixes" | `cargo fmt` |
| Run tests | "Generate test code and execute" | `cargo test` |
| Add dependency | "Edit Cargo.toml with correct syntax" | `cargo add` |
| Fix imports | "Parse file, find missing imports, add them" | `cargo fix --allow-dirty` |

Boitata's tool layer prioritizes these deterministic operations, using the LLM only for:
- **Planning** - Deciding which tools to use
- **Interpretation** - Understanding tool results
- **Complex changes** - Non-routine code modifications that lack deterministic tools

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                          Boitata System                               │
├─────────────────────────────────────────────────────────────────────┤
│                                                                       │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐          │
│  │   CLI/Web    │    │   Blueprint  │    │   Workspace  │          │
│  │   Interface  │───▶│   Executor  │───▶│   Manager    │          │
│  └──────────────┘    └──────────────┘    └──────────────┘          │
│                                │                    │                │
│                       ┌────────▼────────┐         │                │
│                       │  Agent Loop     │         │                │
│                       │  (LLM + Tools)  │◀────────┘                │
│                       └────────┬────────┘                          │
│                                │                                   │
│                       ┌────────▼────────┐                          │
│                       │   Provider      │                          │
│                       │   Layer         │                          │
│                       └────────┬────────┘                          │
│                                │                                   │
│                       ┌────────▼────────┐                          │
│                       │   Extension     │                          │
│                       │   Layer (MCP)   │                          │
│                       └─────────────────┘                          │
└─────────────────────────────────────────────────────────────────────┘
```

## Components

### Provider Layer
Multi-provider LLM abstraction supporting:
- **Anthropic** - Claude models (Sonnet, Opus, Haiku)
- **OpenAI** - GPT models (GPT-4o, GPT-4o-mini)
- **Ollama** - Local models via Ollama

### Agent Loop
The core execution engine that:
1. Accepts a task description
2. Iterates through LLM calls and tool executions
3. Maintains conversation context
4. Returns a structured result

### Tool System
Built-in tools organized by category:

**File System (Deterministic)**
- `file_read` - Read file contents
- `file_write` - Write to files
- `list_directory` - List directory contents

**Code Operations (Deterministic - Planned)**
- `cargo_check` - Run `cargo check`
- `cargo_clippy` - Run `cargo clippy` with auto-fix
- `cargo_fmt` - Format code with `cargo fmt`
- `cargo_test` - Run tests
- `cargo_add` - Add dependencies

**Search (Deterministic)**
- `search` - Code search via ripgrep

**Git (Deterministic)**
- `git_status` - Check git status
- `git_diff` - Show changes
- `git_commit` - Commit changes
- `git_branch` - Manage branches

**Command Execution (Semi-Deterministic)**
- `execute_command` - Run shell commands (deterministic if command is)

### MCP Integration
Planned support for the Model Context Protocol to connect to external tools and data sources.

## Installation

```bash
# Clone the repository
git clone https://github.com/your-username/boitata.git
cd boitata

# Build
cargo build --release

# The binary will be at ./target/release/boitata
```

## Usage

```bash
# Run a task
boitata run "Fix the clippy warnings in src/main.rs"

# Run with a specific blueprint
boitata run --blueprint fix_lint_errors "Fix all lint errors"

# Create a workspace
boitata workspace create /path/to/project

# List tasks
boitata task list
```

## Roadmap

### Sprint 1: Foundation ✅
- [x] Provider trait with multi-provider support
- [x] Agent loop with context management
- [x] Tool registry and first built-in tools
- [x] File system tools (read, write, list)

### Sprint 2: Tools (In Progress)
- [ ] Code operations (cargo check, clippy, fmt, test)
- [ ] Search tools (ripgrep integration)
- [ ] Git operations
- [ ] Command execution with safety checks

### Sprint 3: MCP Integration
- [ ] MCP client implementation
- [ ] Tool discovery and registration
- [ ] Resource access for context gathering

### Sprint 4: Blueprint System
- [ ] Hybrid deterministic/agentic workflows
- [ ] Common blueprints (fix_lint_errors, fix_test_failure, etc.)
- [ ] YAML blueprint definitions

### Sprint 5: Workspace Management
- [ ] Workspace manager with isolation
- [ ] Snapshot/restore for retries
- [ ] Task queue and executor

### Sprint 6: Interfaces
- [ ] Full CLI implementation
- [ ] Web UI for task monitoring
- [ ] Testing integration (Rust-first)

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT License - see LICENSE file for details.

## Acknowledgments

- **Stripe's Minions** - For the blueprint architecture and determinism-first philosophy
- **Block's Goose** - For the modular Rust architecture and MCP integration patterns
- **Anthropic** - For the Claude API and Model Context Protocol

