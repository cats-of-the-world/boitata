# boitata-server

HTTP/SSE backend and embedded web UI for running boitata agent tasks and
blueprints from a browser. Reuses the CLI's runtime assembly
(`boitata_core::runtime`) to build the provider, tools, and policy once, then
serves them to concurrent runs.

## Run

```bash
# 1. Build the web UI (once, or after frontend changes). Output goes to
#    frontend/dist and is embedded into the server binary at compile time.
cd frontend && npm install && npm run build && cd ..

# 2. Build & run the server (reads boitata.toml / $BOITATA_CONFIG like the CLI).
cargo run -p boitata-server -- --addr 127.0.0.1:8787
```

Then open <http://127.0.0.1:8787>.

The backend builds and runs **without** Node — if the UI hasn't been built, the
API still works and the root serves a build hint. For UI development with
hot-reload, run `npm run dev` in `frontend/` (it proxies `/api` to `:8787`).

## API

| Method | Path | Purpose |
| ------ | ---- | ------- |
| `POST` | `/api/runs` | Start a run: `{ "task": "...", "blueprint": "default"? }` → `{ id }` |
| `GET`  | `/api/runs` | List runs (newest first) |
| `GET`  | `/api/runs/{id}` | Run detail: summary, result, full event log |
| `GET`  | `/api/runs/{id}/events` | Live events (Server-Sent Events) |
| `POST` | `/api/runs/{id}/cancel` | Request cancellation |
| `GET`  | `/api/blueprints` | Built-in blueprint names |

Runs are held in memory (v1); restarting the server forgets history.
Human-in-the-loop blueprint nodes are not yet supported over the web.

## Scheduling from the CLI

The `boitata` CLI can schedule a task on a running server instead of executing
locally, streaming the same events to your terminal:

```bash
boitata run "fix the failing test" --remote http://127.0.0.1:8787
boitata run "tidy imports" --blueprint default --remote http://127.0.0.1:8787
```

It POSTs to `/api/runs`, tails `/api/runs/{id}/events`, prints the result, exits
non-zero if the run failed, and cancels the run on Ctrl-C.
