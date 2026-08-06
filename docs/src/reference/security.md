# Security

Boitata runs unattended with the privileges of the process that launches it, so
its safety model matters. Three layers compose to constrain what it can do.

## 1. Tool permission policy

Before every tool call the agent consults a policy. This mirrors the allow/deny
decision Goose's permission layer makes, but it is configured up front because
Boitata runs unattended.

```toml
# "allow_all" (default) or "read_only" (deny any tool that may modify state:
# file_write/file_edit, git_commit, cargo_*, execute_command, ... leaving only
# the read-only tools like file_read, search, git_status).
tool_policy = "read_only"
```

A tool's read-only status comes from its annotations.

### Command denylist

Regexes matched against `execute_command` command strings; a match is denied:

```toml
denied_commands = ['rm\s+-rf\s+/', 'sudo\b', ':\(\)\s*\{']
```

Denied calls never run. The model receives the reason (so it can adapt) and the
denial is recorded in the audit log as a `tool_denied` event. An invalid
`denied_commands` regex is a config error and aborts the run rather than silently
dropping the control.

## 2. Path confinement

The path-taking tools (`file_read`, `file_write`, `list_directory`, `search`)
are confined to a workspace root, which defaults to the directory Boitata runs
in.

- Absolute paths, `..` traversal, and symlinks that escape the root are rejected.
- Point it elsewhere with `workspace_root`.
- Disable confinement entirely with `confine_tools = false`.

```toml
# workspace_root = "/path/to/project"
# confine_tools  = true   # default; set false to allow any path
```

> `execute_command` runs real shell commands and is not bound by this
> confinement. It is enabled by default for full capability; for a locked-down
> deployment, combine confinement with `allow_execute_command = false`.

## 3. Secret handling

Credentials never leak through the machinery:

- `Config::Debug` is implemented by hand to redact `api_key`. Never derive it,
  or the secret leaks anywhere the config is logged or formatted.
- `McpServerConfig` redacts `auth_token` and the values of `env`/`headers`,
  which routinely carry credentials.
- The audit log never contains your API key, and the log file itself is created
  owner-only (`0600` on Unix) because it records full tool arguments and
  results, which routinely include secrets.
- `boitata.toml` (which may hold secrets) is git-ignored by default.
- HTTP API (`boitata-server`): loopback-only by default. Binding a non-loopback
  address requires an API token (`api_token` / `BOITATA_API_TOKEN`), and every
  request must then carry it (`Authorization: Bearer <token>`). Provider
  requests never follow HTTP redirects, so credentials can't be forwarded to a
  redirect target.

## Putting it together

| Control | What it stops | Where |
|---------|---------------|-------|
| `tool_policy = "read_only"` | Any state-mutating tool | all tools |
| `denied_commands` | Specific shell commands | `execute_command` |
| `confine_tools` / `workspace_root` | Access outside the workspace | path-taking tools |
| `allow_execute_command = false` | Arbitrary shell entirely | `execute_command` |

For a read-only deployment, set `tool_policy = "read_only"`. For a locked-down
but still-capable deployment, keep `allow_execute_command` but add
`denied_commands` and confine the path tools.
