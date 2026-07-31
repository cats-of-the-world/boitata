# Quick Start

This page takes you from a fresh build to a completed task in a few minutes.

## 1. Create a config file

Boitata reads its settings from a TOML file. Start from the template:

```bash
cp boitata.example.toml boitata.toml
```

`boitata.toml` is git-ignored, so your API key never gets committed. The CLI
looks for it in the current directory by default; override with `--config <path>`
or the `BOITATA_CONFIG` environment variable.

A minimal config points at a provider, a model, and an endpoint:

```toml
provider = "openai"
model    = "glm-4.6"
base_url = "https://api.z.ai/api/paas/v4/chat/completions"
max_tokens = 4096
```

Every field is documented in the [Configuration reference](../reference/configuration.md).

## 2. Provide an API key

For real secrets, leave `api_key` blank in the file and export it instead — the
environment variable takes precedence over the file:

```bash
export BOITATA_API_KEY="your-key"
```

## 3. Run a task

```bash
# Uses ./boitata.toml
./target/release/boitata run "List the files in the current directory and summarize them"

# Point at a specific config file
./target/release/boitata run --config prod.toml "Read Cargo.toml and tell me the crate name"
```

The agent loops over LLM calls and tool executions until the task is done, then
prints the tool calls it made and a final summary.

## 4. Read the audit log

Every run appends structured events to `boitata-audit.log`. Pretty-print the
latest run with `jq`:

```bash
jq -c . boitata-audit.log
```

See [Audit Log](../reference/audit-log.md) for the full event schema.

## Where to go next

- **Pick a provider** — see [Providers](../reference/providers.md) for Anthropic,
  OpenAI-compatible, and local Ollama setups.
- **Lock it down** — [Security](../reference/security.md) covers the tool
  permission policy and path confinement.
- **Automate a workflow** — [Blueprints](../reference/blueprints.md) stitch
  agent, tool, and script steps into retryable graphs.
- **Run it as a service** — the [Server & Web UI](../interfaces/server.md) page
  covers the HTTP/SSE backend.
