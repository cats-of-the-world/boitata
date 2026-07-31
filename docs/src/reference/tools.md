# Tools

Built-in tools are organized by category. Every tool runs with a timeout and
captured output. On Unix, a timed-out or cancelled command's whole process group
is killed so nothing is orphaned.

## File system (deterministic)

| Tool | Description |
|------|-------------|
| `file_read` | Read file contents (line-numbered; paged with `offset`/`limit`) |
| `file_write` | Create or overwrite a file whole |
| `file_edit` | Replace a unique, exact occurrence of text in a file |
| `list_directory` | List directory contents |

## Code operations (deterministic)

| Tool | Description |
|------|-------------|
| `cargo_check` | Run `cargo check` |
| `cargo_clippy` | Run `cargo clippy` (optional `fix`) |
| `cargo_fmt` | Format code with `cargo fmt` (optional `check`) |
| `cargo_test` | Run tests (optional `filter`) |
| `cargo_add` | Add dependencies (optional `features`, `dev`) |

## Search (deterministic)

| Tool | Description |
|------|-------------|
| `search` | Code search via `ripgrep` |

## Git (deterministic)

| Tool | Description |
|------|-------------|
| `git_status` | Check git status |
| `git_diff` | Show changes (unstaged or `staged`) |
| `git_commit` | Commit changes (optional `all`; never pushes) |
| `git_branch` | List / create / switch branches |

## Command execution (semi-deterministic)

| Tool | Description |
|------|-------------|
| `execute_command` | Run shell commands with the agent's privileges |

Enabled by default — disable with `allow_execute_command = false`.

### Output handling

Every command-based tool keeps its output lean:

- The **tail** of the output is kept in the result.
- When output is truncated, the **full** output is written to a temp file and its
  path is included in the result.
- No interactive stdin.
- Non-zero exits (compiler/linter/test failures) come back as **output**, not
  errors — so the agent can read them and iterate rather than crashing the run.

## How the LLM uses them

The LLM never hand-rolls what a tool can do. It uses the tools for the
mechanical work and spends its budget on planning, interpretation, and complex
changes. See [Determinism First](../concepts/philosophy.md).

## Permission policy

Before every tool call the agent consults a policy. Two composable controls gate
which tools may run — see [Security](./security.md) for the full policy model,
read-only mode, and the command denylist.
