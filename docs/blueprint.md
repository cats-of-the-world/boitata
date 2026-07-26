# Blueprint design

Status: design (Sprint 4). No implementation yet.

## Context

`--blueprint <name>` exists as a CLI flag but is a no-op (`src/main.rs:113`).
Today `run_task` builds one `Agent` and runs a single ReAct loop (LLM + tools).
Sprint 4 wants hybrid deterministic/agentic workflows: a graph whose nodes are
agents, tools, or scripts, with routing that supports conditionals and loops.
Example: before finishing a task, run `cargo fmt`; or, as a first step, create a
devbox. The model is based on LangGraph, adapted so a node can be an agent, a
tool, or a script.

## Model

A blueprint is a graph of nodes and edges over a typed State, executed from an
entry node until END.

### State

Named channels, each with a reducer. Nodes never mutate state directly. A node
returns an update (a set of channel writes); the engine merges each write through
that channel's reducer. This is LangGraph's isolated-state model: what a node
sees is the current state, and what it produces is an update, not a mutation.

Initial channels:

- messages: conversation turns. reducer = append.
- task: the original task string. reducer = set-once.
- status: last node outcome, ok or failed. reducer = last-write.
- vars: string key/value map that nodes emit for routing and templating.
  reducer = merge.

### Nodes

A node is a trait:

```
async fn run(&self, state: &State, cx: &NodeCtx) -> Result<Update>
```

`NodeCtx` carries the cancellation token, the provider, and the tool registry.

Three kinds:

- agent: runs the existing `Agent` (`src/agent/mod.rs`) on a prompt. The prompt
  may template values from `vars`. Update: append messages, set status, and put
  the final message in `vars`.
- tool: invokes a named tool from `ToolRegistry` (`src/tools/mod.rs`) with args.
  Args may template from `vars`. Update: append a tool message, set status, and
  put the tool output text in `vars`.
- script: runs a shell command or script string deterministically, for setup
  steps such as creating a devbox. Reuses the exec infrastructure behind
  `execute_command` (`src/tools/builtins/exec.rs`, `command.rs`): timeout, output
  capture, group-kill on cancel. Update: append output, set status from the exit
  code, and put stdout and exit code in `vars`.

### Edges

- static: from -> to.
- conditional: from -> router(state) -> next node id, or END. Built-in routers:
  branch on status (success or failure), or map a `vars` key to targets.
- START -> entry node. Any node -> END.

### Executor

Sequential with cycles.

- current = entry.
- Loop: run the current node, apply its update via reducers, then route to the
  next node.
- Stop at END, or when a step limit is hit (analogous to LangGraph
  recursion_limit; reuse the existing max-iterations idea).
- Cancellation: the token already threaded through `Agent` and `Tool::execute`
  is passed to each node, so Ctrl-C stops a run promptly.
- Audit: emit per-node start and finish and routing decisions (extend
  `AuditEvent` in `src/audit.rs`).

## Mapping to existing code

- agent node -> `Agent` (`src/agent/mod.rs`).
- tool node -> `ToolRegistry` and `Tool` (`src/tools/mod.rs`).
- script node -> exec infra (`src/tools/builtins/exec.rs`, `command.rs`).
- messages channel -> produces and consumes `Context` (`src/context`) for agent
  nodes.
- cancellation -> existing `CancellationToken` pattern.
- audit -> `AuditSink` and `AuditEvent` (`src/audit.rs`).
- wiring -> `run_task` (`src/main.rs`): with `--blueprint`, build the graph and
  run it through the executor; without it, the current single-agent path, which
  is just the one-node agent blueprint.

## Blueprint definition

Phase 1: blueprints defined in code, in a small registry keyed by name; wire
`--blueprint <name>` to it.

Phase 2: YAML definitions (the README's stated goal). Schema sketch:

```
name: setup_and_fix
entry: devbox
nodes:
  devbox:
    type: script
    run: "devbox init && devbox install"
  fix:
    type: agent
    prompt: "Fix all clippy warnings."
    tools: [file_read, file_edit, search, cargo_clippy]
  fmt:
    type: tool
    tool: cargo_fmt
  verify:
    type: tool
    tool: cargo_check
edges:
  - {from: devbox, to: fix}
  - {from: fix, to: fmt}
  - {from: fmt, to: verify}
  - {from: verify, when: failure, to: fix}
  - {from: verify, when: success, to: END}
```

Note: `serde_yaml` is unmaintained; pick `serde_norway` or `serde_yml` at
implementation time.

## Example: run cargo fmt before finishing

Default blueprint, showing the agent, tool, and loop:

- entry main (agent) -> fmt (tool: cargo_fmt) -> verify (tool: cargo_check).
- verify failure -> main; verify success -> END.

## Phasing

- Phase 1 (foundation): graph model, typed state with reducers, sequential
  executor with conditional edges, cycles, step limit, cancellation, and audit;
  the three node kinds; a code-defined blueprint registry; `--blueprint` wired;
  current single-agent behavior preserved as the default.
- Phase 2: YAML loader plus a starter library (fix_lint_errors, fix_test_failure,
  setup_devbox).
- Phase 3: parallel super-steps (fan-out and fan-in), checkpoint and retry (ties
  into Sprint 5 workspace snapshots), human-in-the-loop interrupts.

## Files (when implemented)

- New: `src/blueprint/mod.rs` (Graph, Node trait, Edge, Executor),
  `src/blueprint/state.rs` (State, channels, reducers),
  `src/blueprint/nodes.rs` (agent, tool, script nodes),
  `src/blueprint/library.rs` (code-defined blueprints).
- Changed: `src/main.rs` (wire `--blueprint`), `src/audit.rs` (node events),
  `README.md` (roadmap).

## Verification (for the implementation phases)

- Unit tests: executor routing, cycles, and step-limit stop; reducers; each node
  kind with a fake provider, tool, and script.
- End to end: run the default blueprint (agent -> cargo_fmt -> cargo_check)
  against this repo; confirm node order and audit trail, and that a failing
  verify loops back to the agent.
