# Boitata

> A fire serpent from Brazilian folklore that protects the forest and brings light to darkness.

**Boitata** is a one-shot, end-to-end coding agent inspired by Stripe's
[Minions](https://stripe.dev/blog/minions-stripes-one-shot-end-to-end-coding-agents-part-1)
and Block's [Goose](https://github.com/block/goose). Built in Rust, it enables
unattended task execution with minimal human intervention.

## What it does

Give Boitata a task in plain language and it will plan, read and edit files,
run commands, and iterate against the compiler and test suite until the work is
done — then hand you back a summary and a full audit trail of every action it
took.

- **Unattended runs.** A task goes in, a result comes out. No mid-run hand-holding.
- **Determinism first.** It leans on real tools (`cargo clippy`, `cargo fmt`,
  `cargo test`, `rg`, `git`) instead of asking the LLM to do everything.
- **Auditable.** Every run appends a structured JSONL event log so you can
  reconstruct exactly what happened.
- **Composable workflows.** *Blueprints* stitch agent, tool, script, and
  human-approval steps into graphs (fan-out, retry, verify loops).
- **Extensible.** Connect any [Model Context Protocol][mcp] server and its tools
  become first-class citizens of the agent loop.

[mcp]: https://modelcontextprotocol.io

## In a hurry?

```bash
git clone https://github.com/cats-of-the-world/boitata.git
cd boitata
cargo build --release
export BOITATA_API_KEY="your-key"
./target/release/boitata run "Summarize the files in the current directory"
```

See [Quick Start](./getting-started/quick-start.md) for the full walkthrough.

## This book

| Section | What you'll find |
|---------|------------------|
| [Getting Started](./getting-started/installation.md) | Install, configure, and run your first task |
| [Concepts](./concepts/philosophy.md) | The why behind the design |
| [Reference](./reference/configuration.md) | Every knob, tool, and integration |
| [Interfaces](./interfaces/server.md) | The HTTP server and web UI |
| [Project](./project/roadmap.md) | Where Boitata is headed |
