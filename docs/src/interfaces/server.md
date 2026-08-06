# Server & Web UI

`boitata-server` is an HTTP/SSE backend with an embedded web UI for running
agent tasks and [blueprints](../reference/blueprints.md) from a browser. It
reuses the CLI's runtime assembly (`boitata_core::runtime`) to build the
provider, tools, and policy once, then serves them to concurrent runs. A task
runs identically from the terminal, the web UI, or the CLI's remote mode.

## Blueprints

The server offers blueprints by name from a directory you point it at with
`--blueprints-dir`. It never reads an arbitrary path from a network request (a
path-traversal risk), so only the vetted files in that directory are runnable.
Each file is compiled at startup, so a malformed blueprint fails fast. Without
the flag, the server runs the single-agent path only and `/api/blueprints` is
empty.

## Run

```bash
# 1. Build the web UI (once, or after frontend changes). Output goes to
#    frontend/dist and is embedded into the server binary at compile time.
cd crates/boitata-server/frontend && npm install && npm run build && cd ..

# 2. Build and run the server (reads boitata.toml / $BOITATA_CONFIG like the CLI).
#    Pass --blueprints-dir to offer blueprints by name in the API and web UI.
cargo run -p boitata-server -- --addr 127.0.0.1:8787 --blueprints-dir examples/blueprints
```

Then open <http://127.0.0.1:8787> and pick a blueprint (or "Single agent") in
the run form.

The backend builds and runs without Node. If the UI hasn't been built, the API
still works and the root serves a build hint. For UI development with hot-reload,
run `npm run dev` in `frontend/` (it proxies `/api` to `:8787`).

## HTTP API

| Method | Path | Purpose |
| ------ | ---- | ------- |
| `POST` | `/api/runs` | Start a run: `{ "task": "...", "blueprint": "name"? }` returns `{ id }`. `blueprint` is a configured name; an unknown name is rejected |
| `GET`  | `/api/runs` | List runs (newest first) |
| `GET`  | `/api/runs/{id}` | Run detail: summary, result, full event log |
| `GET`  | `/api/runs/{id}/events` | Live events (Server-Sent Events) |
| `POST` | `/api/runs/{id}/cancel` | Request cancellation |
| `GET`  | `/api/blueprints` | Names of the configured blueprints (empty without `--blueprints-dir`) |

Runs are held in memory (v1); restarting the server forgets history.

## Scheduling from the CLI

The `boitata` CLI can schedule a task on a running server instead of executing
locally, streaming the same events to your terminal:

```bash
boitata run "fix the failing test" --remote http://127.0.0.1:8787
```

It POSTs to `/api/runs`, tails `/api/runs/{id}/events`, prints the result, exits
non-zero if the run failed, and cancels the run on Ctrl-C. The remote run logs to
the [audit log](../reference/audit-log.md) just like a local one. Over
`--remote`, a `--blueprint` value must be the name of one the server was started
with (`--blueprints-dir`); to run an arbitrary local `.yaml` file, drop
`--remote` and run it locally.
