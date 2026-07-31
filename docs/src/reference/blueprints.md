# Blueprints

A **blueprint** stitches agent, tool, script, and human-approval steps into a
typed graph — hybrid deterministic/agentic workflows with fan-out, retry, and
verify loops. This is how Boitata automates multi-step workflows instead of a
single free-form agent run.

Run one with `--blueprint <name>` (a built-in) or a path to your own `.yaml`:

```bash
# built-in
boitata run "fix the failing test" --blueprint fix_test_failure

# your own file
boitata run "prepare the repo" --blueprint ./my-blueprint.yaml
```

Agent nodes in a blueprint inherit the same provider, tools, policy, and agent
settings the single-agent path uses.

## Schema

A blueprint is a YAML document with a name, an entry node, a map of nodes, and a
list of edges:

```yaml
name: default
entry: main
nodes:
  main:
    type: agent
    prompt: "{task}"
  fmt:
    type: tool
    tool: cargo_fmt
  verify:
    type: script
    run: "cargo check"
edges:
  - {from: main, to: fmt}
  - {from: fmt, to: verify}
  - {from: verify, when: failure, to: main}
  - {from: verify, when: success, to: END}
```

Unknown fields are rejected (`deny_unknown_fields`), so a typo fails loudly at
load time rather than silently misbehaving.

### Node types

Every node is tagged by `type` (snake_case):

| Type | Fields | What it does |
|------|--------|--------------|
| `agent` | `prompt`, `tools?` | Run the LLM agent loop on `prompt`, optionally scoped to a subset of `tools` |
| `tool` | `tool`, `args?` | Invoke a registered tool with arguments (defaults to `{}`) |
| `script` | `run` | Run a shell script deterministically, routing on its exit code |
| `human` | `prompt`, `mode?` | Pause for human input (human-in-the-loop); `mode` defaults to `input` |

### Prompts

`{task}` in an `agent` or `human` prompt is substituted with the run's task
description. This is how a generic blueprint adapts to the task you pass on the
command line.

### Edges

Edges out of a node are either:

- a single unconditional `to`, **or**
- a set of conditional edges each tagged `when: success | failure`.

The loader folds the conditional set into one status router. `to: END` (any case)
routes to the terminal sentinel — that path ends the run.

```yaml
edges:
  - {from: verify, when: failure, to: fix}   # loop back on failure
  - {from: verify, when: success, to: END}   # finish on success
```

Several edges sharing a `when` **fan out** (parallel super-steps); a status with
no edge yields an empty set and that path ends.

## Execution model

- **Parallel super-steps.** Edges that fan out from one node run concurrently
  (fan-out / fan-in).
- **Checkpoint + retry.** A failing super-step can be retried, each attempt
  restoring the pre-step state from an in-memory checkpoint. Bounded by
  `blueprint_max_retries`.
- **Step limit.** Cyclic graphs are bounded by `blueprint_max_steps`.
- **Human-in-the-loop.** A `human` node with `mode: approval` prompts for a
  yes/no; an affirmative reply routes onward, anything else ends the run.
  Requires an interactive stdin — on a non-interactive run the approval node
  errors rather than proceeding unattended.

> Human-in-the-loop nodes are not yet supported over the
> [web UI](../interfaces/server.md).

## Built-in blueprints

Boitata ships starter blueprints in
[`crates/boitata-orchestrator/blueprints/`](https://github.com/cats-of-the-world/boitata/tree/master/crates/boitata-orchestrator/blueprints):

| Blueprint | Shape |
|-----------|-------|
| `default` | `agent → cargo_fmt → cargo check`, looping back on a failed check |
| `fix_lint_errors` | `agent → cargo_fmt → clippy (-D warnings)`, looping until clean |
| `fix_test_failure` | `agent → cargo test`, looping until the suite passes |
| `setup_devbox` | `script (devbox init && install) → agent` |
| `human_approval` | `human (approval) → agent`, ending on a negative reply |

## Writing your own

Drop a `.yaml` file and point `--blueprint` at its path. A common pattern is the
*verify loop*: an `agent` node does the work, a deterministic `script` node
checks it, and a `failure` edge routes back to the agent:

```yaml
name: fmt_then_verify
entry: work
nodes:
  work:
    type: agent
    prompt: |
      Refactor the logging module for clarity. Task context: {task}
  fmt:
    type: tool
    tool: cargo_fmt
  verify:
    type: script
    run: "cargo test && cargo clippy -- -D warnings"
edges:
  - {from: work, to: fmt}
  - {from: fmt, to: verify}
  - {from: verify, when: failure, to: work}
  - {from: verify, when: success, to: END}
```
