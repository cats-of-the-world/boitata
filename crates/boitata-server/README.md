# boitata-server

HTTP/SSE backend and embedded web UI for running boitata agent tasks and
blueprints from a browser. It reuses the CLI's runtime assembly
(`boitata_core::runtime`) to build the provider, tools, and policy once, then
serves them to concurrent runs.

Blueprints are offered by name from the `--blueprints-dir` directory. Only those
vetted files are runnable; the server never reads an arbitrary path from a
network request. Without the flag, the server runs the single-agent path only.

## Run

```bash
# 1. Build the web UI (once, or after frontend changes). Output goes to
#    frontend/dist and is embedded into the server binary at compile time.
cd frontend && npm install && npm run build && cd ..

# 2. Build and run the server (reads boitata.toml / $BOITATA_CONFIG like the CLI).
#    --blueprints-dir offers blueprints by name in the API and web UI.
cargo run -p boitata-server -- --addr 127.0.0.1:8787 --blueprints-dir examples/blueprints
```

Then open <http://127.0.0.1:8787>.

The backend builds and runs without Node. If the UI hasn't been built, the API
still works and the root serves a build hint. For UI development with hot-reload,
run `npm run dev` in `frontend/` (it proxies `/api` to `:8787`).

## API

| Method | Path | Purpose |
| ------ | ---- | ------- |
| `POST` | `/api/runs` | Start a run: `{ "task": "...", "blueprint": "name"? }` returns `{ id }`; an unknown blueprint name is rejected |
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
non-zero if the run failed, and cancels the run on Ctrl-C.
