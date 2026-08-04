import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  type RunDetail,
  type RunEvent,
  type RunResult,
  type RunSummary,
} from "./api.ts";

export function App() {
  const [blueprints, setBlueprints] = useState<string[]>([]);
  const [runs, setRuns] = useState<RunSummary[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .listBlueprints()
      .then(setBlueprints)
      .catch((e) => console.error("failed to load blueprints", e));
  }, []);

  const refreshRuns = useCallback(() => {
    api
      .listRuns()
      .then(setRuns)
      .catch((e) => console.error("failed to refresh runs", e));
  }, []);

  // Poll the run list so statuses stay fresh while runs are in flight.
  useEffect(() => {
    refreshRuns();
    const t = setInterval(refreshRuns, 2000);
    return () => clearInterval(t);
  }, [refreshRuns]);

  const start = useCallback(
    async (task: string, blueprint: string | null) => {
      setError(null);
      try {
        const { id } = await api.startRun(task, blueprint);
        setSelected(id);
        refreshRuns();
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    },
    [refreshRuns],
  );

  return (
    <div className="app">
      <header>
        <h1>🔥 boitata</h1>
        <span className="tagline">agent &amp; orchestrator console</span>
      </header>
      <div className="layout">
        <aside>
          <NewRunForm blueprints={blueprints} onStart={start} />
          {error && <div className="error">{error}</div>}
          <RunList runs={runs} selected={selected} onSelect={setSelected} />
        </aside>
        <main>
          {selected ? (
            <RunView key={selected} id={selected} onChange={refreshRuns} />
          ) : (
            <div className="empty">Select or start a run.</div>
          )}
        </main>
      </div>
    </div>
  );
}

function NewRunForm({
  blueprints,
  onStart,
}: {
  blueprints: string[];
  onStart: (task: string, blueprint: string | null) => void;
}) {
  const [task, setTask] = useState("");
  const [blueprint, setBlueprint] = useState("");

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!task.trim()) return;
    onStart(task.trim(), blueprint || null);
    setTask("");
  };

  return (
    <form className="new-run" onSubmit={submit}>
      <label>Task</label>
      <textarea
        value={task}
        onChange={(e) => setTask(e.target.value)}
        placeholder="Describe the task…"
        rows={4}
      />
      <label>Mode</label>
      <select value={blueprint} onChange={(e) => setBlueprint(e.target.value)}>
        <option value="">Single agent</option>
        {blueprints.map((b) => (
          <option key={b} value={b}>
            blueprint: {b}
          </option>
        ))}
      </select>
      <button type="submit" disabled={!task.trim()}>
        Run
      </button>
    </form>
  );
}

function RunList({
  runs,
  selected,
  onSelect,
}: {
  runs: RunSummary[];
  selected: string | null;
  onSelect: (id: string) => void;
}) {
  return (
    <ul className="run-list">
      {runs.map((r) => (
        <li
          key={r.id}
          className={r.id === selected ? "selected" : ""}
          onClick={() => onSelect(r.id)}
        >
          <span className={`dot ${r.status.state}`} />
          <span className="run-task">{r.task}</span>
          <span className="run-meta">
            {r.blueprint ? `blueprint:${r.blueprint}` : "agent"}
          </span>
        </li>
      ))}
      {runs.length === 0 && <li className="muted">No runs yet.</li>}
    </ul>
  );
}

function RunView({ id, onChange }: { id: string; onChange: () => void }) {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [detail, setDetail] = useState<RunDetail | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const seen = useRef<Set<number>>(new Set());
  const logRef = useRef<HTMLDivElement>(null);

  const push = useCallback((ev: RunEvent) => {
    if (seen.current.has(ev.seq)) return;
    seen.current.add(ev.seq);
    setEvents((prev) => [...prev, ev]);
  }, []);

  useEffect(() => {
    seen.current = new Set();
    setEvents([]);
    setDetail(null);

    let closed = false;
    const es = new EventSource(`/api/runs/${id}/events`);

    const finish = () => {
      if (closed) return;
      closed = true;
      es.close();
      api.getRun(id).then(setDetail).catch(() => {});
      onChange();
    };

    // Consecutive reconnect failures; reset whenever a message arrives so only a
    // sustained outage (not occasional blips over a long run) trips the cap.
    let errors = 0;
    const maxErrors = 5;
    es.onmessage = (msg) => {
      errors = 0;
      let ev: RunEvent;
      try {
        ev = JSON.parse(msg.data) as RunEvent;
      } catch {
        return; // skip a malformed/partial frame rather than break the stream
      }
      push(ev);
      if (ev.event === "run_completed" || ev.event === "blueprint_completed") {
        finish();
      }
    };
    // EventSource auto-reconnects on error. That's what we want for a transient
    // blip, but the server also closes the stream when the run ends — so on each
    // error, fetch the final state and stop once the run is no longer running.
    // Cap consecutive failures so a persistent error can't loop forever.
    es.onerror = () => {
      if (++errors > maxErrors) {
        finish();
        return;
      }
      api
        .getRun(id)
        .then((d) => {
          setDetail(d);
          if (d.status.state !== "running") finish();
        })
        .catch(() => {});
    };

    return () => {
      closed = true;
      es.close();
    };
  }, [id, push, onChange]);

  // Follow the tail only when the user is already near the bottom, so scrolling
  // up to read earlier entries isn't interrupted by new events.
  useEffect(() => {
    const el = logRef.current;
    if (!el) return;
    const nearBottom =
      el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    if (nearBottom) el.scrollTo({ top: el.scrollHeight });
  }, [events]);

  const status = detail?.status.state ?? "running";
  const result = detail?.result ?? null;

  return (
    <div className="run-view">
      <div className="run-header">
        <span className={`dot ${status}`} />
        <strong>{status}</strong>
        {status === "running" && (
          <button
            className="cancel"
            onClick={() =>
              api
                .cancelRun(id)
                .then(() => setActionError(null))
                .catch((e) => setActionError(String(e)))
            }
          >
            Cancel
          </button>
        )}
      </div>
      {actionError && <div className="error">{actionError}</div>}
      <div className="log" ref={logRef}>
        {events.map((ev) => (
          <EventLine key={ev.seq} ev={ev} />
        ))}
      </div>
      {result ? (
        <ResultBox result={result} />
      ) : (
        // No structured result (e.g. a hard error before any step recorded one):
        // still surface the run's final error so a failure is never silent.
        status === "failed" &&
        detail?.status.error && (
          <div className="result fail">
            <h3>Failed</h3>
            <pre className="error">{detail.status.error}</pre>
          </div>
        )
      )}
    </div>
  );
}

function EventLine({ ev }: { ev: RunEvent }) {
  const { icon, text, cls } = useMemo(() => formatEvent(ev), [ev]);
  return (
    <div className={`event ${cls}`}>
      <span className="icon">{icon}</span>
      <span className="text">{text}</span>
    </div>
  );
}

function ResultBox({ result }: { result: RunResult }) {
  return (
    <div className={`result ${result.success ? "ok" : "fail"}`}>
      <h3>{result.success ? "Completed" : "Failed"}</h3>
      {result.error && <p className="error">{result.error}</p>}
      {result.final_message && <pre>{result.final_message}</pre>}
      {result.transcript.length > 0 && (
        <div className="transcript">
          {result.transcript.map((t, i) => (
            <div key={i}>
              <strong>[{t.node}]</strong>
              <pre>{t.text}</pre>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// Human-readable one-liner per audit event kind.
function formatEvent(ev: RunEvent): {
  icon: string;
  text: string;
  cls: string;
} {
  const s = (k: string) => String(ev[k] ?? "");
  switch (ev.event) {
    case "run_started":
      return { icon: "▶", text: `run started · ${s("provider")}/${s("model")}`, cls: "info" };
    case "llm_response": {
      // On the `llm_response` audit event `tool_calls` is a list of tool *names*
      // (Vec<String>), not the ToolCall objects carried by a run's result.
      const tools = (ev.tool_calls as string[] | undefined) ?? [];
      const suffix = tools.length ? ` · tools: ${tools.join(", ")}` : "";
      return { icon: "🧠", text: `iteration ${s("iteration")}${suffix}`, cls: "info" };
    }
    case "tool_call":
      return {
        icon: ev.is_error ? "✗" : "✓",
        text: `${s("name")}(${truncate(s("arguments"), 80)}) → ${truncate(s("result"), 160)}`,
        cls: ev.is_error ? "err" : "ok",
      };
    case "tool_denied":
      return { icon: "⛔", text: `${s("name")} denied · ${s("reason")}`, cls: "err" };
    case "context_compacted":
      return { icon: "🗜", text: `context compacted (${s("tokens_before")}→${s("tokens_after")} tok)`, cls: "muted" };
    case "run_completed":
      return {
        icon: ev.success ? "■" : "✗",
        text: `run ${ev.success ? "completed" : "failed"} · ${s("iterations")} iteration(s)`,
        cls: ev.success ? "ok" : "err",
      };
    case "blueprint_started":
      return { icon: "▶", text: `blueprint ${s("blueprint")} · entry ${s("entry")}`, cls: "info" };
    case "node_executed": {
      const status = s("status");
      const output = s("output");
      const base = `[${s("node")}] ${s("kind")} → ${s("next")} (${status})`;
      return {
        icon: "◆",
        text: output ? `${base} · ${truncate(output, 200)}` : base,
        cls: status === "failed" ? "err" : "info",
      };
    }
    case "super_step_retried":
      return { icon: "↻", text: `retry #${s("attempt")} at step ${s("step")} · ${s("error")}`, cls: "err" };
    case "blueprint_completed":
      return { icon: "■", text: `blueprint finished · ${s("steps")} step(s) · ${s("reason")}`, cls: "ok" };
    default:
      return { icon: "•", text: ev.event, cls: "muted" };
  }
}

function truncate(s: string, n: number): string {
  return s.length > n ? s.slice(0, n) + "…" : s;
}
