# Audit Log

Every run appends structured events to a JSONL audit log (default
`boitata-audit.log`, configurable via `audit_log` in the config file).

Each line is one JSON object tagged with a per-run `run_id` and an RFC 3339
timestamp, so you can reconstruct exactly what happened — including failures on
unattended runs. The log is git-ignored and never contains your API key.

## Event types

| Event | When | Notable fields |
|-------|------|----------------|
| `run_started` | Run begins | `task`, `provider`, `model` |
| `llm_response` | Each LLM call returns | token usage |
| `tool_call` | Each tool call | `arguments`, `result`, `read_only` flag |
| `tool_denied` | A tool call was blocked by the policy | the reason |
| `run_completed` | Run finishes | `success`/`error`, `iterations`, token totals |

The `read_only` flag on `tool_call` comes from the tool's annotations, so you
can tell observing calls from mutating ones at a glance.

## Reading the log

Pretty-print with `jq`:

```bash
jq -c . boitata-audit.log
```

Filter to one run by its `run_id`:

```bash
jq -c 'select(.run_id == "c880…")' boitata-audit.log
```

## Example lines

```jsonl
{"run_id":"c880…","timestamp":"2026-07-24T09:04:08+00:00","event":"run_started","task":"hello world","provider":"ollama","model":"llama3.2"}
{"run_id":"c880…","timestamp":"2026-07-24T09:04:08+00:00","event":"run_completed","success":false,"iterations":1,"error":"Provider error: …connection refused…","total_input_tokens":0,"total_output_tokens":0}
```

## MCP and remote runs

MCP tool calls go through the same agent loop as built-in tools, so **MCP tool
calls show up in the audit log too**. Tasks scheduled on a remote
[server](../interfaces/server.md) log identically to local runs.
