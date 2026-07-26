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
- `file_read` - Read file contents (line-numbered; paged with `offset`/`limit`)
- `file_write` - Create or overwrite a file whole
- `file_edit` - Replace a unique, exact occurrence of text in a file
- `list_directory` - List directory contents

**Code Operations (Deterministic)**
- `cargo_check` - Run `cargo check`
- `cargo_clippy` - Run `cargo clippy` (optional `fix`)
- `cargo_fmt` - Format code with `cargo fmt` (optional `check`)
- `cargo_test` - Run tests (optional `filter`)
- `cargo_add` - Add dependencies (optional `features`, `dev`)

**Search (Deterministic)**
- `search` - Code search via ripgrep

**Git (Deterministic)**
- `git_status` - Check git status
- `git_diff` - Show changes (unstaged or `staged`)
- `git_commit` - Commit changes (optional `all`; never pushes)
- `git_branch` - List / create / switch branches

**Command Execution (Semi-Deterministic)**
- `execute_command` - Run shell commands; runs with the agent's privileges.
  Enabled by default — disable with `allow_execute_command = false`

Every command-based tool runs with a timeout, captured output (the tail is kept
to keep the context lean; when output is truncated the **full** output is written
to a temp file and its path is included in the result), and no interactive stdin.
On Unix, a timed-out or cancelled command's whole process group is killed so
nothing is orphaned. Non-zero exits (compiler/linter/test failures) come back as
output — not errors — so the agent can read them and iterate.

**Path confinement (secure by default).** The path-taking tools (`file_read`,
`file_write`, `list_directory`, `search`) are confined to a workspace root —
by default the directory Boitata runs in. Absolute paths, `..` traversal, and
symlinks that escape the root are rejected. Point it elsewhere with
`workspace_root`, or disable confinement entirely with `confine_tools = false`.
Note that `execute_command` runs real shell commands and is **not** bound by this
confinement. It's enabled by default for full capability; for a locked-down
deployment, combine the confinement with `allow_execute_command = false`.

### MCP Integration
Planned support for the Model Context Protocol to connect to external tools and data sources.

## Installation

```bash
# Clone the repository
git clone https://github.com/cats-of-the-world/boitata.git
cd boitata

# Build
cargo build --release

# The binary will be at ./target/release/boitata
```

## Development setup

Boitata needs a Rust toolchain plus a couple of external tools: `ripgrep` (for
the `search` tool) and `git` (for the `git_*` tools). To set up a new machine
deterministically:

```bash
./scripts/setup.sh
```

This installs the exact Rust toolchain pinned in `rust-toolchain.toml`, installs
the pinned `ripgrep` version, and checks for `git`. Crate versions are pinned by
the committed `Cargo.lock`, and CI builds with the same pinned toolchain (rustup
reads `rust-toolchain.toml` automatically).

## Configuration

Boitata reads its settings from a TOML file. Copy the template and fill in your
credentials:

```bash
cp boitata.example.toml boitata.toml
```

`boitata.toml` is git-ignored so your API key never gets committed. The CLI
looks for it in the current directory by default; override with `--config <path>`
or the `BOITATA_CONFIG` environment variable.

Minimal config:

```toml
provider = "openai"   # "anthropic" | "openai" | "ollama"
model    = "glm-4.6"
api_key  = "your-key"
base_url = "https://api.z.ai/api/paas/v4/chat/completions"
max_tokens = 4096
```

For real secrets, leave `api_key` blank in the file and export it instead — the
env var takes precedence:

```bash
export BOITATA_API_KEY="your-key"
```

## Usage

```bash
# Build
cargo build --release

# Run a task (uses ./boitata.toml)
./target/release/boitata run "List the files in the current directory and summarize them"

# Point at a specific config file
./target/release/boitata run --config prod.toml "Read Cargo.toml and tell me the crate name"
```

The agent loops over LLM calls and tool executions until the task is done, then
prints the tool calls it made and a final summary.

### Audit log

Every run appends structured events to a JSONL audit log (default
`boitata-audit.log`, configurable via `audit_log` in the config file). Each line
is one JSON object tagged with a per-run `run_id` and an RFC 3339 timestamp, so
you can reconstruct exactly what happened — including failures on unattended
runs. The log is git-ignored and never contains your API key.

Event types: `run_started`, `llm_response` (with token usage), `tool_call`
(with arguments, result, and a `read_only` flag from the tool's annotations so
you can tell observing calls from mutating ones), and `run_completed`
(success/error + token totals).

```jsonl
{"run_id":"c880…","timestamp":"2026-07-24T09:04:08+00:00","event":"run_started","task":"hello world","provider":"ollama","model":"llama3.2"}
{"run_id":"c880…","timestamp":"2026-07-24T09:04:08+00:00","event":"run_completed","success":false,"iterations":1,"error":"Provider error: …connection refused…","total_input_tokens":0,"total_output_tokens":0}
```

Pretty-print the latest run with `jq`:

```bash
jq -c . boitata-audit.log
```

### Example: testing with z.ai (GLM models)

[z.ai](https://z.ai) exposes both an OpenAI-compatible and an Anthropic-compatible
API, so either provider works against it.

**OpenAI-compatible** (recommended — matches the `openai` provider exactly):

```toml
provider = "openai"
model    = "glm-4.6"
base_url = "https://api.z.ai/api/paas/v4/chat/completions"
max_tokens = 4096
```

**Anthropic-compatible** (uses the native Anthropic Messages API):

```toml
provider = "anthropic"
model    = "glm-4.6"
base_url = "https://api.z.ai/api/anthropic/v1/messages"
max_tokens = 4096
```

Then:

```bash
export BOITATA_API_KEY="your-z.ai-key"
./target/release/boitata run "Say hello and confirm the connection works"
```

> Note: the `anthropic` provider currently authenticates with the `x-api-key`
> header. If z.ai's Anthropic endpoint rejects it in favor of `Authorization:
> Bearer`, prefer the OpenAI-compatible config above.

### Local models with Ollama

No API key required — just point at a running Ollama instance:

```toml
provider = "ollama"
model    = "llama3.2"
base_url = "http://localhost:11434"
```

### MCP servers

Boitata connects to [MCP](https://modelcontextprotocol.io) servers using the
official [`rmcp`](https://crates.io/crates/rmcp) client. Each server's tools are
discovered at startup and exposed to the agent — namespaced as `<server>_<tool>`
— and called through the same agent loop as built-in tools (so **MCP tool calls
show up in the audit log** too). A server that fails to start is logged and
skipped, so one broken server can't abort a run.

Two transports are supported, inferred from which field you set on a
`[[mcp_servers]]` block:

- **`command`** → **stdio**: the server is spawned as a subprocess.
- **`url`** → **Streamable HTTP**: connect to a remote server.

Set exactly one of the two per server.

```toml
# stdio (subprocess)
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "git"
command = "uvx"
args = ["mcp-server-git"]
env = { GIT_AUTHOR_NAME = "boitata" }

# remote (Streamable HTTP)
[[mcp_servers]]
name = "remote"
url = "https://mcp.example.com/mcp"
auth_token = "your-token"          # sent as `Authorization: Bearer <token>`
headers = { X-Workspace = "acme" } # optional extra headers
```

On startup you'll see a line like `MCP server \`filesystem\` connected: 12 tool(s)`.
Credentials (`auth_token`) live in the git-ignored `boitata.toml` and are never logged.

**Resources.** When a server advertises the MCP *resources* capability, Boitata
also registers two tools per server so the agent can gather context on demand:
`<server>_list_resources` (returns the available resource URIs, names, and
descriptions) and `<server>_read_resource` (reads one by URI). These count toward
the tool total reported at startup and, like tool calls, appear in the audit log.

## Roadmap

### Sprint 1: Foundation ✅
- [x] Provider trait with multi-provider support
- [x] Agent loop with context management
- [x] Tool registry and first built-in tools
- [x] File system tools (read, write, list)

### Sprint 2: Tools ✅
- [x] Code operations (cargo check, clippy, fmt, test, add)
- [x] Search tools (ripgrep integration)
- [x] Git operations (status, diff, commit, branch)
- [x] Command execution with safety checks (timeout, output cap, opt-out)

### Sprint 3: MCP Integration ✅
- [x] MCP client implementation (via `rmcp`)
- [x] Tool discovery and registration
- [x] Remote transport (Streamable HTTP) + stdio
- [x] Resource access for context gathering

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

