// Thin client for the boitata-server JSON/SSE API. Types mirror the server's
// wire structs (see crates/boitata-server/src/{state,events}.rs).

export type RunState = "running" | "succeeded" | "failed" | "cancelled";

export interface RunStatus {
  state: RunState;
  error?: string | null;
}

export interface RunSummary {
  id: string;
  task: string;
  blueprint: string | null;
  status: RunStatus;
  started_at: string;
}

export interface ToolCall {
  name: string;
  arguments: string;
  result: string;
  is_error: boolean;
}

export interface TranscriptEntry {
  node: string;
  text: string;
}

export interface RunResult {
  success: boolean;
  final_message: string | null;
  error: string | null;
  iterations: number | null;
  tool_calls: ToolCall[];
  transcript: TranscriptEntry[];
}

// How a blueprint node executes (mirrors orchestrator's `Execution`).
export type Execution = "probabilistic" | "deterministic" | "human";

export interface ConfigField {
  key: string;
  value: string;
}

export interface BlueprintNode {
  id: string;
  kind: string;
  execution: Execution;
  detail: string | null;
  config: ConfigField[];
}

export interface BlueprintEdge {
  from: string;
  to: string; // node id or "END"
  when: string | null; // "success" | "failure" | null
}

export interface BlueprintGraph {
  name: string;
  entry: string;
  nodes: BlueprintNode[];
  edges: BlueprintEdge[];
}

// One live event: `seq` plus the flattened audit event, whose kind is in `event`.
export interface RunEvent {
  seq: number;
  event: string;
  [key: string]: unknown;
}

export interface RunDetail extends RunSummary {
  result: RunResult | null;
  events: RunEvent[];
}

// Throw a consistent Error for any non-2xx response, preferring the server's
// `{ error }` body when present. Used by both JSON and empty-body endpoints.
async function assertOk(res: Response): Promise<Response> {
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error((body as { error?: string }).error ?? `HTTP ${res.status}`);
  }
  return res;
}

async function getJson<T>(url: string): Promise<T> {
  const res = await assertOk(await fetch(url));
  return res.json() as Promise<T>;
}

// Runs are keyed by server-issued UUIDs, but encode defensively so a path
// segment can never be crafted from an id.
const run = (id: string) => `/api/runs/${encodeURIComponent(id)}`;

export const api = {
  listBlueprints: () => getJson<string[]>("/api/blueprints"),
  getBlueprint: (name: string) =>
    getJson<BlueprintGraph>(`/api/blueprints/${encodeURIComponent(name)}`),
  listRuns: () => getJson<RunSummary[]>("/api/runs"),
  getRun: (id: string) => getJson<RunDetail>(run(id)),
  startRun: async (task: string, blueprint: string | null) => {
    const res = await assertOk(
      await fetch("/api/runs", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ task, blueprint: blueprint || undefined }),
      }),
    );
    return res.json() as Promise<{ id: string }>;
  },
  cancelRun: async (id: string) => {
    await assertOk(await fetch(`${run(id)}/cancel`, { method: "POST" }));
  },
};
