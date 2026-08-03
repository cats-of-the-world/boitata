# Server & Web UI

`boitata-server` is an HTTP/SSE backend with an embedded web UI for running
agent tasks from a browser. It reuses the CLI's runtime assembly
(`boitata_core::runtime`) to build the provider, tools, and policy once, then
serves them to concurrent runs — so a task runs identically from the terminal,
the web UI, or the CLI's remote mode.

The server runs the single-agent path only: [blueprints](../reference/blueprints.md)
are user-provided YAML files loaded from disk, which the server won't read from a
network request (a path-traversal risk), so run those locally with the CLI's
`--blueprint <path>`.

## Run

```bash
# 1. Build the web UI (once, or after frontend changes). Output goes to
#    frontend/dist and is embedded into the server binary at compile time.
cd crates/boitata-server/frontend && npm install && npm run build && cd ..

# 2. Build & run the server (reads boitata.toml / $BOITATA_CONFIG like the CLI).
cargo run -p boitata-server -- --addr 127.0.0.1:8787
```

Then open <http://127.0.0.1:8787>.

The backend builds and runs **without** Node — if the UI hasn't been built, the
API still works and the root serves a build hint. For UI development with
hot-reload, run `npm run dev` in `frontend/` (it proxies `/api` to `:8787`).

## HTTP API

| Method | Path | Purpose |
| ------ | ---- | ------- |
| `POST` | `/api/runs` | Start a run: `{ "task": "..." }` → `{ id }` (a `blueprint` field is rejected) |
| `GET`  | `/api/runs` | List runs (newest first) |
| `GET`  | `/api/runs/{id}` | Run detail: summary, result, full event log |
| `GET`  | `/api/runs/{id}/events` | Live events (Server-Sent Events) |
| `POST` | `/api/runs/{id}/cancel` | Request cancellation |
| `GET`  | `/api/blueprints` | Blueprints the server can run by name (always empty; see above) |

Runs are held in memory (v1); restarting the server forgets history.

## Scheduling from the CLI

The `boitata` CLI can schedule a task on a running server instead of executing
locally, streaming the same events to your terminal:

```bash
boitata run "fix the failing test" --remote http://127.0.0.1:8787
```

It POSTs to `/api/runs`, tails `/api/runs/{id}/events`, prints the result, exits
non-zero if the run failed, and cancels the run on Ctrl-C. The remote run logs
to the [audit log](../reference/audit-log.md) just like a local one. Remote runs
take the single-agent path; `--blueprint` runs locally only (see above).
