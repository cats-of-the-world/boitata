# Determinism First

A core principle of Boitata is to **use deterministic tools whenever possible**
instead of asking the LLM to do everything.

## Why

Asking a language model to hand-edit code that a formatter, linter, or test
runner can already handle is slow, expensive, and unreliable. A deterministic
tool:

- **Reduces token costs** — it runs without an LLM round-trip.
- **Improves reliability** — deterministic operations have predictable outcomes.
- **Enables faster iteration** — quick feedback loops without API calls.
- **Maintains consistency** — the same inputs always produce the same outputs.

## What gets offloaded

| Task | LLM approach | Deterministic approach |
|------|--------------|------------------------|
| Fix lint errors | "Read file, identify errors, fix them manually" | `cargo clippy --fix` |
| Format code | "Identify formatting issues and apply fixes" | `cargo fmt` |
| Run tests | "Generate test code and execute" | `cargo test` |
| Add dependency | "Edit Cargo.toml with correct syntax" | `cargo add` |
| Fix imports | "Parse file, find missing imports, add them" | `cargo fix --allow-dirty` |

## What the LLM still does

Boitata's tool layer prioritizes these deterministic operations, using the LLM
only for what genuinely needs judgment:

- **Planning** — deciding which tools to use and in what order.
- **Interpretation** — understanding tool results and deciding what to do next.
- **Complex changes** — non-routine code modifications that lack a deterministic
  tool.

This division is what lets Boitata run *unattended*: the boring, mechanical,
error-prone steps are pinned to deterministic tools, and the LLM spends its
budget on the parts that actually need it.

This philosophy also shapes the [Blueprint](../reference/blueprints.md) system,
where a deterministic `script` node (e.g. `cargo test`) can gate an `agent` node
in a retry loop — the agent only re-runs when the deterministic check fails.
