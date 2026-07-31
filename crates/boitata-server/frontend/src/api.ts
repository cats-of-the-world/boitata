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

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error((body as { error?: string }).error ?? `HTTP ${res.status}`);
  }
  return res.json() as Promise<T>;
}

export const api = {
  listBlueprints: () => fetch("/api/blueprints").then((r) => json<string[]>(r)),
  listRuns: () => fetch("/api/runs").then((r) => json<RunSummary[]>(r)),
  getRun: (id: string) => fetch(`/api/runs/${id}`).then((r) => json<RunDetail>(r)),
  startRun: (task: string, blueprint: string | null) =>
    fetch("/api/runs", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ task, blueprint: blueprint || undefined }),
    }).then((r) => json<{ id: string }>(r)),
  cancelRun: (id: string) =>
    fetch(`/api/runs/${id}/cancel`, { method: "POST" }),
};
