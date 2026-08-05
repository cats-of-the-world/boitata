import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  type BlueprintGraph,
  type BlueprintNode,
  type Execution,
  type RunDetail,
  type RunEvent,
  type RunResult,
  type RunSummary,
} from "./api.ts";

export function App() {
  const [blueprints, setBlueprints] = useState<string[]>([]);
  const [runs, setRuns] = useState<RunSummary[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  // The blueprint chosen in the form, previewed as a graph until a run starts.
  const [preview, setPreview] = useState<string>("");
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
        <span className="mark" aria-hidden="true">
          🔥
        </span>
        <h1>boitatá</h1>
        <span className="tagline">agent &amp; orchestrator console</span>
      </header>
      <div className="layout">
        <aside>
          <NewRunForm
            blueprints={blueprints}
            onStart={start}
            onPreview={(bp) => {
              setPreview(bp);
              if (bp) setSelected(null); // show the graph, not a prior run
            }}
          />
          {error && <div className="error">{error}</div>}
          <RunList runs={runs} selected={selected} onSelect={setSelected} />
        </aside>
        <main>
          {selected ? (
            <RunView key={selected} id={selected} onChange={refreshRuns} />
          ) : preview ? (
            <BlueprintGraphView name={preview} />
          ) : (
            <div className="empty">
              Select or start a run, or pick a blueprint to preview its graph.
            </div>
          )}
        </main>
      </div>
    </div>
  );
}

function NewRunForm({
  blueprints,
  onStart,
  onPreview,
}: {
  blueprints: string[];
  onStart: (task: string, blueprint: string | null) => void;
  onPreview: (blueprint: string) => void;
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
      <select
        value={blueprint}
        onChange={(e) => {
          setBlueprint(e.target.value);
          onPreview(e.target.value);
        }}
      >
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
  const groups = useMemo(() => groupEvents(events), [events]);
  // The freshest step is the one worth keeping open; older ones fold away.
  const lastKey = groups.length ? groups[groups.length - 1].key : null;

  return (
    <div className="run-view">
      <div className="run-header">
        <span className={`dot ${status}`} />
        <strong className="run-status">{status}</strong>
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
      {status === "running" && <RunningBanner />}
      {actionError && <div className="error">{actionError}</div>}
      <div className="log" ref={logRef}>
        {groups.map((g) => (
          <StepGroup
            key={g.key}
            group={g}
            // Keep the active/most-recent step open; collapse finished ones so a
            // long run stays scannable.
            defaultOpen={g.key === lastKey || g.status === "running"}
          />
        ))}
        {groups.length === 0 && (
          <div className="muted">Waiting for the first event…</div>
        )}
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

// Live feedback while a run is in flight: the boitatá (a fiery serpent of
// Brazilian folklore) coils along, making it obvious the agent is still working
// rather than stalled.
function RunningBanner() {
  return (
    <div className="running-banner" role="status" aria-live="polite">
      <span className="serpent" aria-hidden="true">
        🐍
      </span>
      <span className="running-text">Boitatá is working…</span>
      <span className="embers" aria-hidden="true">
        <i /> <i /> <i />
      </span>
    </div>
  );
}

// A run's event stream, folded into the logical steps it moved through. Each
// group is collapsible so a finished step can be tucked away while the active
// one stays visible.
interface StepGroupData {
  key: string;
  title: string;
  status: "info" | "ok" | "err" | "running";
  events: RunEvent[];
}

// Split the flat audit stream into steps. A blueprint run breaks on each executed
// node (and on retries); a single-agent run is one step that closes when the run
// completes. A trailing, unclosed buffer is the step currently in progress.
function groupEvents(events: RunEvent[]): StepGroupData[] {
  const isBlueprint = events.some((e) => e.event === "blueprint_started");
  const boundary = new Set(
    isBlueprint
      ? ["blueprint_started", "node_executed", "super_step_retried", "blueprint_completed"]
      : ["run_completed"],
  );

  const groups: StepGroupData[] = [];
  let buf: RunEvent[] = [];
  let n = 0;
  const flush = (running: boolean) => {
    if (buf.length === 0) return;
    groups.push(makeGroup(buf, n++, running));
    buf = [];
  };
  for (const ev of events) {
    buf.push(ev);
    if (boundary.has(ev.event)) flush(false);
  }
  flush(true); // whatever is left is the step still underway
  return groups;
}

// Derive a step group's title and status from the events it holds.
function makeGroup(evs: RunEvent[], i: number, running: boolean): StepGroupData {
  const last = evs[evs.length - 1];
  const s = (k: string) => String(last[k] ?? "");
  let title = "Step";
  let status: StepGroupData["status"] = running ? "running" : "info";
  switch (last.event) {
    case "blueprint_started":
      title = `Blueprint · ${s("blueprint")}`;
      status = "info";
      break;
    case "node_executed":
      title = s("node") || "step";
      status = s("status") === "failed" ? "err" : "ok";
      break;
    case "super_step_retried":
      title = `Retry · step ${s("step")}`;
      status = "err";
      break;
    case "blueprint_completed":
      title = "Blueprint finished";
      status = "ok";
      break;
    case "run_completed":
      title = "Agent";
      status = last.success ? "ok" : "err";
      break;
    default:
      title = running ? "Working…" : "Agent";
  }
  return { key: `g${i}`, title, status, events: evs };
}

function StepGroup({
  group,
  defaultOpen,
}: {
  group: StepGroupData;
  defaultOpen: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className={`step step-${group.status}`}>
      <button
        type="button"
        className="step-head"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <span className="step-caret">{open ? "▾" : "▸"}</span>
        <span className={`dot ${group.status === "running" ? "running" : ""}`} />
        <span className="step-title">{group.title}</span>
        <span className="step-count">
          {group.events.length} event{group.events.length === 1 ? "" : "s"}
        </span>
      </button>
      {open && (
        <div className="step-body">
          {group.events.map((ev) => (
            <EventLine key={ev.seq} ev={ev} />
          ))}
        </div>
      )}
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

// How each execution class is labelled in the graph and legend.
const EXEC: Record<Execution, { icon: string; label: string }> = {
  probabilistic: { icon: "🎲", label: "probabilistic (LLM)" },
  deterministic: { icon: "⚙️", label: "deterministic" },
  human: { icon: "🧑", label: "human-in-the-loop" },
};

// Fetch a blueprint's graph and render it: the layered graph, a legend explaining
// the marking, and the configuration of the selected step.
function BlueprintGraphView({ name }: { name: string }) {
  const [graph, setGraph] = useState<BlueprintGraph | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  // "graph" draws the layered diagram; "source" shows the raw YAML definition.
  const [view, setView] = useState<"graph" | "source">("graph");

  useEffect(() => {
    // Guard against a stale response: if `name` changes before the fetch
    // resolves, ignore the earlier result so it can't overwrite the newer graph.
    let cancelled = false;
    setGraph(null);
    setErr(null);
    setSelected(null);
    api
      .getBlueprint(name)
      .then((g) => {
        if (cancelled) return;
        setGraph(g);
        setSelected(g.entry); // show the entry step's config by default
      })
      .catch((e) => {
        if (cancelled) return;
        setErr(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [name]);

  if (err) return <div className="error">Failed to load blueprint: {err}</div>;
  if (!graph) return <div className="empty">Loading blueprint…</div>;

  const selectedNode = graph.nodes.find((n) => n.id === selected) ?? null;

  return (
    <div className="bp">
      <div className="bp-title">
        <strong>{graph.name}</strong>
        <span className="muted">entry: {graph.entry}</span>
        <div className="bp-tabs">
          <button
            type="button"
            className={view === "graph" ? "active" : ""}
            onClick={() => setView("graph")}
          >
            Graph
          </button>
          <button
            type="button"
            className={view === "source" ? "active" : ""}
            onClick={() => setView("source")}
          >
            Definition
          </button>
        </div>
      </div>
      {view === "graph" ? (
        <>
          <div className="bp-legend">
            {(Object.keys(EXEC) as Execution[]).map((k) => (
              <span key={k} className={`bp-chip exec-${k}`}>
                {EXEC[k].icon} {EXEC[k].label}
              </span>
            ))}
          </div>
          <BlueprintGraphSvg
            graph={graph}
            selected={selected}
            onSelect={setSelected}
          />
          <NodeConfig node={selectedNode} />
        </>
      ) : (
        <BlueprintSourceView name={name} />
      )}
    </div>
  );
}

// The blueprint's raw definition (the YAML file as written), fetched on demand so
// the graph view isn't slowed by a second request that most viewers won't open.
function BlueprintSourceView({ name }: { name: string }) {
  const [source, setSource] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setSource(null);
    setErr(null);
    api
      .getBlueprintSource(name)
      .then((s) => {
        if (!cancelled) setSource(s.source);
      })
      .catch((e) => {
        if (!cancelled) setErr(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [name]);

  if (err) return <div className="error">Failed to load definition: {err}</div>;
  if (source === null) return <div className="empty">Loading definition…</div>;
  return (
    <pre className="bp-source">
      <code>{source}</code>
    </pre>
  );
}

// The configuration of one step: its execution class, kind, and every parameter
// (prompt, command, image, …) exactly as written in the blueprint.
function NodeConfig({ node }: { node: BlueprintNode | null }) {
  if (!node) {
    return <div className="bp-config muted">Click a step to see its configuration.</div>;
  }
  return (
    <div className="bp-config">
      <div className="bp-config-head">
        <span className={`bp-chip exec-${node.execution}`}>
          {EXEC[node.execution].icon} {EXEC[node.execution].label}
        </span>
        <strong>{node.id}</strong>
        <code className="bp-config-kind">{node.kind}</code>
      </div>
      {node.config.length === 0 ? (
        <div className="muted">No configuration.</div>
      ) : (
        <dl className="bp-config-fields">
          {node.config.map((c) => (
            <div key={c.key} className="bp-config-field">
              <dt>{c.key}</dt>
              <dd>
                <pre>{c.value}</pre>
              </dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  );
}

// Node/layout geometry (px).
const NODE_W = 172;
const NODE_H = 60;
const COL_GAP = 40;
const ROW = NODE_H + 52; // top-to-top distance between layers
const PAD = 24;

// A layered top-down drawing of the blueprint: HTML node cards positioned over an
// SVG edge layer. Layers come from a breadth-first walk from the entry node, so a
// run reads top-to-bottom; edges that go back up (verify loops) curve on the
// right. Deterministic vs probabilistic is shown by colour + badge on each card.
function BlueprintGraphSvg({
  graph,
  selected,
  onSelect,
}: {
  graph: BlueprintGraph;
  selected: string | null;
  onSelect: (id: string) => void;
}) {
  const { positions, width, height, hasEnd } = useMemo(() => {
    const hasEnd = graph.edges.some((e) => e.to === "END");
    const ids = graph.nodes.map((n) => n.id);
    const allIds = hasEnd ? [...ids, "END"] : ids;

    // BFS layering from the entry; unreached nodes land on layer 0.
    const layerOf = new Map<string, number>([[graph.entry, 0]]);
    const queue = [graph.entry];
    while (queue.length) {
      const u = queue.shift() as string;
      const lu = layerOf.get(u) ?? 0;
      for (const e of graph.edges) {
        if (e.from === u && !layerOf.has(e.to)) {
          layerOf.set(e.to, lu + 1);
          queue.push(e.to);
        }
      }
    }
    for (const id of allIds) if (!layerOf.has(id)) layerOf.set(id, 0);

    const layers: string[][] = [];
    for (const id of allIds) {
      const l = layerOf.get(id) as number;
      (layers[l] ||= []).push(id);
    }
    for (let i = 0; i < layers.length; i++) layers[i] ||= [];

    const maxCols = Math.max(1, ...layers.map((r) => r.length));
    const width = PAD * 2 + maxCols * NODE_W + (maxCols - 1) * COL_GAP;
    const height = PAD * 2 + (layers.length - 1) * ROW + NODE_H;

    const positions = new Map<string, { x: number; y: number; layer: number }>();
    layers.forEach((row, l) => {
      const rowWidth = row.length * NODE_W + (row.length - 1) * COL_GAP;
      const startX = (width - rowWidth) / 2;
      row.forEach((id, i) => {
        positions.set(id, {
          x: startX + i * (NODE_W + COL_GAP),
          y: PAD + l * ROW,
          layer: l,
        });
      });
    });
    return { positions, width, height, hasEnd };
  }, [graph]);

  // Build an SVG path for one edge: straight-ish down for a forward edge, a curve
  // bulging right for a back/lateral edge (a loop). Returns the path and a point
  // to anchor the success/failure label.
  const edgeGeom = (from: string, to: string) => {
    const a = positions.get(from);
    const b = positions.get(to);
    if (!a || !b) return null;
    const ax = a.x + NODE_W / 2;
    const bx = b.x + NODE_W / 2;
    if (b.layer > a.layer) {
      const ay = a.y + NODE_H;
      const by = b.y;
      const my = (ay + by) / 2;
      return {
        d: `M ${ax} ${ay} C ${ax} ${my}, ${bx} ${my}, ${bx} ${by}`,
        label: { x: (ax + bx) / 2, y: my },
      };
    }
    // Back or lateral edge: leave the right side and re-enter the target's right.
    const ay = a.y + NODE_H / 2;
    const by = b.y + NODE_H / 2;
    const ar = a.x + NODE_W;
    const br = b.x + NODE_W;
    const bulge = Math.max(ar, br) + 46;
    return {
      d: `M ${ar} ${ay} C ${bulge} ${ay}, ${bulge} ${by}, ${br} ${by}`,
      label: { x: bulge - 6, y: (ay + by) / 2 },
    };
  };

  const whenClass = (when: string | null) => {
    if (when === "success") return "ok";
    if (when === "failure") return "err";
    return "plain";
  };

  return (
    <div className="bp-canvas" style={{ width, height }}>
      <svg className="bp-edges" width={width} height={height}>
        <defs>
          {["plain", "ok", "err"].map((c) => (
            <marker
              key={c}
              id={`arrow-${c}`}
              viewBox="0 0 10 10"
              refX="9"
              refY="5"
              markerWidth="7"
              markerHeight="7"
              orient="auto-start-reverse"
            >
              <path d="M 0 0 L 10 5 L 0 10 z" className={`arrow ${c}`} />
            </marker>
          ))}
        </defs>
        {graph.edges.map((e, i) => {
          const g = edgeGeom(e.from, e.to);
          if (!g) return null;
          const cls = whenClass(e.when);
          return (
            <g key={i}>
              <path d={g.d} className={`edge ${cls}`} markerEnd={`url(#arrow-${cls})`} />
              {e.when && (
                <text x={g.label.x} y={g.label.y} className={`edge-label ${cls}`}>
                  {e.when === "success" ? "✓" : "✗"}
                </text>
              )}
            </g>
          );
        })}
      </svg>
      {graph.nodes.map((n) => {
        const p = positions.get(n.id);
        if (!p) return null;
        return (
          <div
            key={n.id}
            className={`bp-node exec-${n.execution}${n.id === selected ? " selected" : ""}`}
            style={{ left: p.x, top: p.y, width: NODE_W, height: NODE_H }}
            title={n.detail ? `${n.kind}: ${n.detail}` : n.kind}
            onClick={() => onSelect(n.id)}
          >
            <div className="bp-node-head">
              <span className="bp-node-badge">{EXEC[n.execution].icon}</span>
              <span className="bp-node-id">{n.id}</span>
            </div>
            <div className="bp-node-kind">
              {n.kind}
              {n.detail ? ` · ${truncate(n.detail, 20)}` : ""}
            </div>
          </div>
        );
      })}
      {hasEnd &&
        (() => {
          const p = positions.get("END");
          if (!p) return null;
          return (
            <div
              className="bp-node bp-end"
              style={{ left: p.x, top: p.y, width: NODE_W, height: NODE_H }}
            >
              END
            </div>
          );
        })()}
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
