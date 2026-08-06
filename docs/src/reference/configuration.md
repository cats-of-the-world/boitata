# Configuration

Boitata reads its settings from a TOML file (default: `boitata.toml` in the
current directory) so credentials never need to be passed on the command line.
The file is git-ignored; commit `boitata.example.toml` as a template instead.

## Finding the config file

The CLI resolves the config file in this order (later wins):

1. `./boitata.toml`
2. the path passed to `--config <path>`
3. the path in the `BOITATA_CONFIG` environment variable

Start from the template:

```bash
cp boitata.example.toml boitata.toml
```

## Secrets

Prefer environment variables for real secrets. They take precedence over the
file and keep credentials off disk:

```bash
export BOITATA_API_KEY="your-key"
```

| Env var | Overrides | Purpose |
|---------|-----------|---------|
| `BOITATA_CONFIG` | config file path | Point at a non-default config |
| `BOITATA_API_KEY` | `api_key` | API key (takes precedence over the file) |

> The `Config` struct implements `Debug` by hand to redact `api_key`, and
> `McpServerConfig` redacts `auth_token` plus the values of `env`/`headers`. The
> secrets are never written to the audit log either.

## Fields

### Core

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `provider` | string | none | `"anthropic"`, `"openai"`, or `"ollama"` |
| `model` | string | none | Model identifier (e.g. `"glm-4.6"`) |
| `api_key` | string? | none | API key. Prefer `BOITATA_API_KEY` |
| `base_url` | string? | provider default | Override the provider's endpoint |
| `max_tokens` | usize? | conservative | Output token budget per request |
| `max_iterations` | usize? | 50 | Maximum agent iterations before giving up |
| `system_prompt` | string? | sensible default | Custom system prompt for the agent |

### Logging

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `audit_log` | string? | `boitata-audit.log` | Path to the JSONL audit log |

See [Audit Log](./audit-log.md) for what gets written.

### Context management

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `auto_compact_threshold` | float? | `0.8` | Fraction of the context window at which older turns are summarized. `0.0` disables compaction |

### Tools and security

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `allow_execute_command` | bool? | `true` | Register the arbitrary `execute_command` tool |
| `workspace_root` | string? | current dir | Root that path-taking tools are confined to |
| `confine_tools` | bool? | `true` | Confine path-taking tools to `workspace_root` |
| `tool_policy` | string? | `"allow_all"` | `"allow_all"` or `"read_only"` |
| `denied_commands` | [string]? | `[]` | Regexes; a matching `execute_command` is denied |

See [Security](./security.md) for how these compose.

### MCP servers

A list of `[[mcp_servers]]` blocks. Each server's tools are discovered at
startup and exposed to the agent. See [MCP Servers](./mcp.md).

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Namespacing identifier (e.g. `"git"`) |
| `command` | string? | Executable to spawn (selects the stdio transport) |
| `args` | [string] | Arguments for `command` |
| `env` | map | Extra env vars for the server process |
| `url` | string? | Remote endpoint (selects the Streamable HTTP transport) |
| `auth_token` | string? | Bearer token for HTTP |
| `headers` | map | Extra HTTP headers |

> Set exactly one of `command` or `url` per server. Setting both, or neither, is
> a config error that aborts the run.

### Blueprints

Only used with `--blueprint`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `blueprint_max_steps` | usize? | executor limit | Cap on super-steps (bounds cyclic graphs) |
| `blueprint_max_retries` | usize? | `0` | Times to retry a failed super-step, restoring pre-step state |

See [Blueprints](./blueprints.md).

## Complete example

```toml
provider   = "openai"
model      = "glm-4.6"
base_url   = "https://api.z.ai/api/paas/v4/chat/completions"
max_tokens = 4096

# audit_log          = "boitata-audit.log"
# max_iterations     = 50
# auto_compact_threshold = 0.8

# --- security ---
# allow_execute_command = true
# confine_tools         = true
# workspace_root        = "/path/to/project"
# tool_policy           = "read_only"
# denied_commands       = ['rm\s+-rf\s+/', 'sudo\b']

# --- MCP servers ---
# [[mcp_servers]]
# name    = "filesystem"
# command = "npx"
# args    = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```
