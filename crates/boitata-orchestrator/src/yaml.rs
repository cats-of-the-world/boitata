// YAML blueprint loader: parse a blueprint definition into a runnable [`Graph`].
//
// The on-disk schema mirrors the graph model (nodes over a typed state, edges
// with optional conditions):
//
//   name: fix_lint_errors
//   entry: fix
//   nodes:
//     fix:    { type: agent,  prompt: "Fix all clippy warnings. {task}" }
//     fmt:    { type: tool,   tool: cargo_fmt }
//     verify: { type: script, run: "cargo clippy -- -D warnings" }
//   edges:
//     - { from: fix, to: fmt }
//     - { from: fmt, to: verify }
//     - { from: verify, when: failure, to: fix }
//     - { from: verify, when: success, to: END }
//
// Edges out of a node are either a single unconditional `to`, or a set of
// conditional edges each tagged `when: success | failure`; the loader folds the
// conditional set into one status router. `to: END` (any case) routes to the
// [`END`] sentinel.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::container::{AgentSandboxNode, CheckoutNode, ExecNode, ProvisionNode};
use super::nodes::{AgentNode, HumanMode, HumanNode, ScriptNode, ToolNode};
use super::state::Status;
use super::{END, Graph, GraphBuilder};

/// A blueprint as written in YAML, before compilation into a [`Graph`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlueprintDef {
    name: String,
    entry: String,
    /// Node id -> its definition. The map key is the node's name.
    nodes: HashMap<String, NodeDef>,
    #[serde(default)]
    edges: Vec<EdgeDef>,
}

/// One node, tagged by `type`. Fields are per-kind; the node's name is the key
/// it is stored under in `nodes`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum NodeDef {
    /// Run the LLM agent loop on `prompt`, optionally scoped to `tools`.
    Agent {
        prompt: String,
        #[serde(default)]
        tools: Option<Vec<String>>,
    },
    /// Invoke a registered tool with optional `args`.
    Tool {
        tool: String,
        #[serde(default = "empty_args")]
        args: Value,
    },
    /// Run a shell script deterministically, routing on its exit code.
    Script { run: String },
    /// Pause for human input (human-in-the-loop). `mode` defaults to `input`.
    Human {
        prompt: String,
        #[serde(default)]
        mode: HumanMode,
    },
    /// Create an ephemeral container from `image`. Its output (the container id)
    /// is stored under the node name for downstream `{name}` references. `env` is
    /// a list of environment variable *names* to forward from the orchestrator's
    /// environment into the container (values are read at run time — never written
    /// in the blueprint), e.g. an API key an in-container agent needs.
    Provision {
        image: String,
        #[serde(default)]
        env: Vec<String>,
    },
    /// Git-clone `repo` into `container` (a `{node}` reference to a provision
    /// node). `ref` and `path` are optional (`path` defaults to `/workspace`).
    Checkout {
        container: String,
        repo: String,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    /// Run a shell command inside `container`, routing on its exit code.
    Exec {
        container: String,
        run: String,
        #[serde(default)]
        workdir: Option<String>,
    },
    /// Run the agent *inside* `container` over ACP: launch the agent server there,
    /// connect, and stream its events into the blueprint.
    AgentSandbox {
        container: String,
        prompt: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        command: Option<String>,
    },
}

/// Tool args default to an empty object rather than `null`, since tools expect
/// an object of arguments.
fn empty_args() -> Value {
    Value::Object(serde_json::Map::new())
}

/// How a node executes, for visualizing a blueprint: whether its step is
/// **probabilistic** (an LLM decides what happens) or **deterministic** (a fixed
/// tool/script/container step), with human-in-the-loop called out on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Execution {
    /// Driven by the LLM (an `agent` / `agent_sandbox` node) — the outcome varies.
    Probabilistic,
    /// A fixed operation (tool, script, or container step) — same inputs, same run.
    Deterministic,
    /// Pauses for a human decision (a `human` node).
    Human,
}

impl NodeDef {
    /// The node's schema `type` tag, for display.
    fn kind_str(&self) -> &'static str {
        match self {
            NodeDef::Agent { .. } => "agent",
            NodeDef::Tool { .. } => "tool",
            NodeDef::Script { .. } => "script",
            NodeDef::Human { .. } => "human",
            NodeDef::Provision { .. } => "provision",
            NodeDef::Checkout { .. } => "checkout",
            NodeDef::Exec { .. } => "exec",
            NodeDef::AgentSandbox { .. } => "agent_sandbox",
        }
    }

    /// Whether this node's step is probabilistic (LLM), deterministic, or human.
    fn execution(&self) -> Execution {
        match self {
            NodeDef::Agent { .. } | NodeDef::AgentSandbox { .. } => Execution::Probabilistic,
            NodeDef::Human { .. } => Execution::Human,
            NodeDef::Tool { .. }
            | NodeDef::Script { .. }
            | NodeDef::Provision { .. }
            | NodeDef::Checkout { .. }
            | NodeDef::Exec { .. } => Execution::Deterministic,
        }
    }

    /// A one-line summary of what the node does, for the graph tooltip (the tool
    /// name, the command, the image/repo, …). `None` for agent nodes, whose work
    /// is the free-form prompt.
    fn detail(&self) -> Option<String> {
        match self {
            NodeDef::Agent { .. } | NodeDef::AgentSandbox { .. } => None,
            NodeDef::Tool { tool, .. } => Some(tool.clone()),
            NodeDef::Script { run } => Some(run.clone()),
            NodeDef::Human { prompt, .. } => Some(prompt.clone()),
            NodeDef::Provision { image, .. } => Some(image.clone()),
            NodeDef::Checkout { repo, .. } => Some(repo.clone()),
            NodeDef::Exec { run, .. } => Some(run.clone()),
        }
    }

    /// The node's full configuration as ordered key/value fields, for showing the
    /// exact parameters of each step (prompt, command, image, repo, port, …).
    /// Optional fields are included only when set.
    fn config(&self) -> Vec<ConfigField> {
        let f = |key: &str, value: String| ConfigField {
            key: key.to_string(),
            value,
        };
        match self {
            NodeDef::Agent { prompt, tools } => {
                let mut c = vec![f("prompt", prompt.clone())];
                if let Some(tools) = tools {
                    c.push(f("tools", tools.join(", ")));
                }
                c
            }
            NodeDef::Tool { tool, args } => {
                let mut c = vec![f("tool", tool.clone())];
                let empty = args.as_object().is_some_and(|o| o.is_empty());
                if !args.is_null() && !empty {
                    // Pretty JSON for readability; fall back to the compact form
                    // (both are infallible for a `Value`) rather than an empty
                    // string, so the field is never silently blanked.
                    let rendered =
                        serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
                    c.push(f("args", rendered));
                }
                c
            }
            NodeDef::Script { run } => vec![f("run", run.clone())],
            NodeDef::Human { prompt, mode } => {
                // Match explicitly rather than deriving from `Debug`, so the wire
                // string doesn't silently change if a variant is renamed.
                let mode = match mode {
                    HumanMode::Input => "input",
                    HumanMode::Approval => "approval",
                };
                vec![f("prompt", prompt.clone()), f("mode", mode.to_string())]
            }
            NodeDef::Provision { image, env } => {
                let mut c = vec![f("image", image.clone())];
                // Only the variable *names* are shown — the blueprint never holds
                // their (possibly secret) values.
                if !env.is_empty() {
                    c.push(f("env", env.join(", ")));
                }
                c
            }
            NodeDef::Checkout {
                container,
                repo,
                git_ref,
                path,
            } => {
                let mut c = vec![f("container", container.clone()), f("repo", repo.clone())];
                if let Some(r) = git_ref {
                    c.push(f("ref", r.clone()));
                }
                if let Some(p) = path {
                    c.push(f("path", p.clone()));
                }
                c
            }
            NodeDef::Exec {
                container,
                run,
                workdir,
            } => {
                let mut c = vec![f("container", container.clone()), f("run", run.clone())];
                if let Some(w) = workdir {
                    c.push(f("workdir", w.clone()));
                }
                c
            }
            NodeDef::AgentSandbox {
                container,
                prompt,
                port,
                command,
            } => {
                let mut c = vec![
                    f("container", container.clone()),
                    f("prompt", prompt.clone()),
                ];
                if let Some(p) = port {
                    c.push(f("port", p.to_string()));
                }
                if let Some(cmd) = command {
                    c.push(f("command", cmd.clone()));
                }
                c
            }
        }
    }
}

/// A blueprint's shape for display: its nodes (each tagged deterministic vs
/// probabilistic) and edges (with any `success`/`failure` condition). Derived
/// straight from the YAML so conditional branches are visible — unlike the
/// compiled [`Graph`], whose routers are opaque closures.
#[derive(Debug, Serialize)]
pub struct BlueprintGraph {
    pub name: String,
    pub entry: String,
    pub nodes: Vec<BlueprintNodeInfo>,
    pub edges: Vec<BlueprintEdgeInfo>,
}

/// One node in a [`BlueprintGraph`].
#[derive(Debug, Serialize)]
pub struct BlueprintNodeInfo {
    pub id: String,
    /// The schema `type` (`agent`, `tool`, `script`, …).
    pub kind: String,
    pub execution: Execution,
    /// A short summary of the node's work (tool name, command, image, …).
    pub detail: Option<String>,
    /// The node's full configuration as ordered key/value fields.
    pub config: Vec<ConfigField>,
}

/// One `key: value` field of a node's configuration (e.g. `prompt`, `run`,
/// `image`), for showing the exact parameters of each step.
#[derive(Debug, Serialize)]
pub struct ConfigField {
    pub key: String,
    pub value: String,
}

/// One edge in a [`BlueprintGraph`]. `to` is a node id or the string `"END"`;
/// `when` is `"success"`/`"failure"` for a conditional edge, else `None`.
#[derive(Debug, Serialize)]
pub struct BlueprintEdgeInfo {
    pub from: String,
    pub to: String,
    pub when: Option<String>,
}

/// Describe a blueprint's shape from its YAML source, for visualization. Parses
/// the document (so a schema error — a bad field or node `type` — is caught), but
/// does *not* compile it, so graph-level checks (unknown edge targets, routing
/// conflicts) are not applied — see [`from_yaml`] for those. Callers that only
/// describe blueprints they've already loaded (e.g. the server, which compiles
/// every blueprint at startup) get both. Nodes are ordered entry-first, then by
/// name, so the output is stable.
pub fn describe(src: &str) -> anyhow::Result<BlueprintGraph> {
    let def: BlueprintDef =
        serde_norway::from_str(src).context("failed to parse blueprint YAML")?;
    let mut nodes: Vec<BlueprintNodeInfo> = def
        .nodes
        .iter()
        .map(|(id, node)| BlueprintNodeInfo {
            id: id.clone(),
            kind: node.kind_str().to_string(),
            execution: node.execution(),
            detail: node.detail(),
            config: node.config(),
        })
        .collect();
    // Entry first (it's where a run begins), then alphabetical — a stable order
    // that reads top-down, independent of the YAML map's iteration order.
    nodes.sort_by(|a, b| {
        let rank = |id: &str| usize::from(id != def.entry);
        rank(&a.id).cmp(&rank(&b.id)).then_with(|| a.id.cmp(&b.id))
    });
    let edges = def
        .edges
        .iter()
        .map(|e| BlueprintEdgeInfo {
            from: e.from.clone(),
            to: if e.to.eq_ignore_ascii_case("END") {
                "END".to_string()
            } else {
                e.to.clone()
            },
            when: e.when.clone(),
        })
        .collect();
    Ok(BlueprintGraph {
        name: def.name,
        entry: def.entry,
        nodes,
        edges,
    })
}

/// One edge. Either unconditional (`to` only) or conditional (`when` + `to`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeDef {
    from: String,
    to: String,
    /// `success` or `failure`; absent for an unconditional edge.
    #[serde(default)]
    when: Option<String>,
}

/// Parse a blueprint definition from YAML text and compile it into a [`Graph`].
pub fn from_yaml(src: &str) -> anyhow::Result<Graph> {
    let def: BlueprintDef =
        serde_norway::from_str(src).context("failed to parse blueprint YAML")?;
    def.into_graph()
}

impl BlueprintDef {
    fn into_graph(self) -> anyhow::Result<Graph> {
        // Node names, captured before the map is consumed, so edge targets
        // (including conditional ones, which `build` can't see) are validated here.
        let known: HashSet<String> = self.nodes.keys().cloned().collect();
        let mut builder = Graph::builder(self.name, self.entry);
        for (name, node) in self.nodes {
            builder = add_node(builder, name, node);
        }
        builder = add_edges(builder, self.edges, &known)?;
        builder.build().map_err(Into::into)
    }
}

/// Add one node definition to the builder under `name`.
fn add_node(builder: GraphBuilder, name: String, node: NodeDef) -> GraphBuilder {
    match node {
        NodeDef::Agent { prompt, tools } => {
            let mut agent = AgentNode::new(name, prompt);
            if let Some(tools) = tools {
                agent = agent.with_tools(tools);
            }
            builder.node(agent)
        }
        NodeDef::Tool { tool, args } => builder.node(ToolNode::new(name, tool, args)),
        NodeDef::Script { run } => builder.node(ScriptNode::new(name, run)),
        NodeDef::Human { prompt, mode } => builder.node(HumanNode::new(name, prompt, mode)),
        NodeDef::Provision { image, env } => builder.node(ProvisionNode::new(name, image, env)),
        NodeDef::Checkout {
            container,
            repo,
            git_ref,
            path,
        } => builder.node(CheckoutNode::new(name, container, repo, git_ref, path)),
        NodeDef::Exec {
            container,
            run,
            workdir,
        } => builder.node(ExecNode::new(name, container, run, workdir)),
        NodeDef::AgentSandbox {
            container,
            prompt,
            port,
            command,
        } => builder.node(AgentSandboxNode::new(
            name, container, prompt, port, command,
        )),
    }
}

/// Group edges by source and install each group as either a static edge or a
/// status-conditional router.
fn add_edges(
    mut builder: GraphBuilder,
    edges: Vec<EdgeDef>,
    known: &HashSet<String>,
) -> anyhow::Result<GraphBuilder> {
    // Preserve source order for stable, deterministic errors.
    let mut order: Vec<String> = Vec::new();
    let mut by_from: HashMap<String, Vec<EdgeDef>> = HashMap::new();
    for edge in edges {
        if !by_from.contains_key(&edge.from) {
            order.push(edge.from.clone());
        }
        by_from.entry(edge.from.clone()).or_default().push(edge);
    }

    for from in order {
        let group = by_from.remove(&from).expect("from was recorded in order");

        // Validate the group's *structure* before any per-edge target check, so
        // the most actionable error wins regardless of edge order: an unknown
        // source, then a conditional/unconditional mix, come first.
        if !known.contains(&from) {
            bail!("edge source `{from}` is not a node");
        }
        let has_conditional = group.iter().any(|e| e.when.is_some());
        let has_unconditional = group.iter().any(|e| e.when.is_none());
        if has_conditional && has_unconditional {
            bail!(
                "node `{from}` mixes conditional (`when`) and unconditional edges; make them all conditional"
            );
        }

        if has_conditional {
            builder = add_conditional(builder, &from, group, known)?;
        } else {
            // No `when`: one edge is the common case; several fan out to all of
            // the targets (they run together in the next super-step).
            for edge in group {
                let to = normalize_target(&edge.to);
                builder = builder.edge(from.clone(), to);
            }
        }
    }
    Ok(builder)
}

/// Fold a group of `when`-tagged edges out of `from` into one router that maps
/// the last node's status to its successor set. Several edges sharing a `when`
/// fan out; a status with no edge yields an empty set (that path ends).
fn add_conditional(
    builder: GraphBuilder,
    from: &str,
    group: Vec<EdgeDef>,
    known: &HashSet<String>,
) -> anyhow::Result<GraphBuilder> {
    let mut on_success: Vec<String> = Vec::new();
    let mut on_failure: Vec<String> = Vec::new();
    for edge in group {
        // `add_edges` rejects mixed groups, so every edge here carries a `when`.
        let when = edge
            .when
            .as_deref()
            .expect("add_edges routes only all-conditional groups here");
        let target = normalize_target(&edge.to);
        // `build` only checks static edge targets; conditional ones are checked
        // here so a typo fails at load time rather than mid-run.
        if target != END && !known.contains(&target) {
            bail!("edge `{from}` -> `{target}` (when: {when}) targets an unknown node");
        }
        match when {
            "success" => on_success.push(target),
            "failure" => on_failure.push(target),
            other => bail!("edge from `{from}` has unknown `when: {other}` (use success|failure)"),
        }
    }

    // The `when` set matching the last node's status is the successor set; a
    // missing status (only before any node has run) is treated as success. END
    // entries are dropped by the executor, so an all-END set ends the path.
    Ok(builder.conditional(from.to_string(), move |state| {
        match state.status {
            Some(Status::Failed) => &on_failure,
            _ => &on_success,
        }
        .clone()
    }))
}

/// Map the schema's `END` (any case) to the [`END`] sentinel; other targets are
/// node ids, kept verbatim.
fn normalize_target(to: &str) -> String {
    if to.eq_ignore_ascii_case("END") {
        END.to_string()
    } else {
        to.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name: sample
entry: fix
nodes:
  fix:
    type: agent
    prompt: "Fix it: {task}"
    tools: [file_read, file_edit]
  fmt:
    type: tool
    tool: cargo_fmt
  verify:
    type: script
    run: "cargo check"
edges:
  - {from: fix, to: fmt}
  - {from: fmt, to: verify}
  - {from: verify, when: failure, to: fix}
  - {from: verify, when: success, to: END}
"#;

    #[test]
    fn describe_classifies_nodes_and_keeps_edges() {
        let g = describe(SAMPLE).expect("valid blueprint");
        assert_eq!(g.entry, "fix");
        // Entry first, then alphabetical.
        assert_eq!(
            g.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["fix", "fmt", "verify"]
        );
        // The agent node is probabilistic; the tool/script nodes deterministic.
        let by = |id: &str| g.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(by("fix").execution, Execution::Probabilistic);
        assert_eq!(by("fmt").execution, Execution::Deterministic);
        assert_eq!(by("fmt").detail.as_deref(), Some("cargo_fmt"));
        assert_eq!(by("verify").kind, "script");
        // Each node carries its full configuration as ordered fields.
        let cfg = |id: &str, key: &str| {
            by(id)
                .config
                .iter()
                .find(|c| c.key == key)
                .map(|c| c.value.as_str())
        };
        assert_eq!(cfg("verify", "run"), Some("cargo check"));
        assert_eq!(cfg("fix", "prompt"), Some("Fix it: {task}"));
        assert_eq!(cfg("fix", "tools"), Some("file_read, file_edit"));
        // Conditional edges keep their `when`, and END is rendered as "END".
        let end = g
            .edges
            .iter()
            .find(|e| e.from == "verify" && e.to == "END")
            .unwrap();
        assert_eq!(end.when.as_deref(), Some("success"));
        assert!(
            g.edges.iter().any(|e| e.from == "verify"
                && e.to == "fix"
                && e.when.as_deref() == Some("failure"))
        );
    }

    #[test]
    fn describe_rejects_invalid_yaml() {
        assert!(describe("not: [a blueprint").is_err());
    }

    #[test]
    fn describe_provision_shows_env_names_only() {
        // A provision node's `env` lists variable *names*; the config view shows
        // those names (never values, which the blueprint doesn't contain).
        let src = r#"
name: c
entry: box
nodes:
  box: {type: provision, image: "img", env: [ANTHROPIC_API_KEY, OPENAI_API_KEY]}
edges:
  - {from: box, to: END}
"#;
        let g = describe(src).unwrap();
        let node = g.nodes.iter().find(|n| n.id == "box").unwrap();
        let env = node
            .config
            .iter()
            .find(|c| c.key == "env")
            .expect("env field in config");
        assert_eq!(env.value, "ANTHROPIC_API_KEY, OPENAI_API_KEY");
    }

    #[test]
    fn parses_and_compiles_a_full_blueprint() {
        let graph = from_yaml(SAMPLE).expect("valid blueprint");
        // Static edges resolve to their targets; the conditional router branches
        // on status: failure -> fix, success -> END.
        assert_eq!(graph.route_for_test("fix"), ["fmt"]);
        assert_eq!(graph.route_for_test("fmt"), ["verify"]);
        assert_eq!(
            graph.route_with_status_for_test("verify", Some(Status::Failed)),
            ["fix"]
        );
        assert_eq!(
            graph.route_with_status_for_test("verify", Some(Status::Ok)),
            [END]
        );
    }

    #[test]
    fn parses_human_node_modes() {
        // Exercises the YAML path for the `human` variant: `mode: approval`, the
        // default `mode` (input), and the `HumanMode` snake_case rename.
        let src = r#"
name: h
entry: approve
nodes:
  approve: {type: human, mode: approval, prompt: "ok? {task}"}
  ask: {type: human, prompt: "your name?"}
edges:
  - {from: approve, when: success, to: ask}
  - {from: approve, when: failure, to: END}
  - {from: ask, to: END}
"#;
        let graph = from_yaml(src).unwrap();
        assert_eq!(
            graph.route_with_status_for_test("approve", Some(Status::Ok)),
            ["ask"]
        );
        assert_eq!(
            graph.route_with_status_for_test("approve", Some(Status::Failed)),
            [END]
        );
        assert_eq!(graph.route_for_test("ask"), [END]);
    }

    #[test]
    fn rejects_unknown_human_mode() {
        // `deny_unknown_fields` plus the mode enum reject a bad mode value.
        let src = r#"
name: h
entry: a
nodes:
  a: {type: human, mode: shout, prompt: "hi"}
"#;
        assert!(from_yaml(src).is_err());
    }

    #[test]
    fn end_target_is_case_insensitive() {
        let src = r#"
name: t
entry: a
nodes:
  a: {type: tool, tool: cargo_fmt}
edges:
  - {from: a, to: end}
"#;
        let graph = from_yaml(src).unwrap();
        assert_eq!(graph.route_for_test("a"), [END]);
    }

    #[test]
    fn tool_args_default_to_empty_object() {
        let src = r#"
name: t
entry: a
nodes:
  a: {type: tool, tool: cargo_fmt}
"#;
        // A tool node with no `args` compiles (args default to `{}`), and a node
        // with no outgoing edge ends the run.
        let graph = from_yaml(src).unwrap();
        assert_eq!(graph.route_for_test("a"), [END]);
    }

    #[test]
    fn rejects_unknown_when_value() {
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
edges:
  - {from: a, when: maybe, to: END}
"#;
        let err = from_yaml(src).err().unwrap().to_string();
        assert!(err.contains("unknown `when"), "{err}");
    }

    #[test]
    fn repeated_when_fans_out() {
        // Several edges sharing a `when` fan out on that status.
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
  b: {type: script, run: "true"}
  c: {type: script, run: "true"}
edges:
  - {from: a, when: failure, to: b}
  - {from: a, when: failure, to: c}
"#;
        let graph = from_yaml(src).unwrap();
        let mut routes = graph.route_with_status_for_test("a", Some(Status::Failed));
        routes.sort();
        assert_eq!(routes, ["b", "c"]);
    }

    #[test]
    fn multiple_unconditional_edges_fan_out() {
        // Several unconditional edges from one node fan out to all targets.
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
  b: {type: script, run: "true"}
  c: {type: script, run: "true"}
edges:
  - {from: a, to: b}
  - {from: a, to: c}
"#;
        let graph = from_yaml(src).unwrap();
        let mut routes = graph.route_for_test("a");
        routes.sort();
        assert_eq!(routes, ["b", "c"]);
    }

    #[test]
    fn mixing_error_wins_over_edge_order() {
        // A group that mixes a conditional edge (with a bad target, listed first)
        // and an unconditional edge must report the structural mixing error, not
        // the unknown-target error — regardless of edge order.
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
  b: {type: script, run: "true"}
edges:
  - {from: a, when: failure, to: nowhere}
  - {from: a, to: b}
"#;
        let err = from_yaml(src).err().unwrap().to_string();
        assert!(err.contains("mixes conditional"), "{err}");
    }

    #[test]
    fn rejects_edge_from_unknown_node() {
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
edges:
  - {from: ghost, to: a}
"#;
        let err = from_yaml(src).err().unwrap().to_string();
        assert!(err.contains("edge source `ghost`"), "{err}");
    }

    #[test]
    fn rejects_conditional_target_that_is_not_a_node() {
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
edges:
  - {from: a, when: failure, to: nowhere}
  - {from: a, when: success, to: END}
"#;
        let err = from_yaml(src).err().unwrap().to_string();
        assert!(err.contains("nowhere"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        // A typo in a schema key is a load-time error, not a silently ignored
        // field. Covers the top-level struct, an edge, and a node (tagged enum).
        let top = r#"
name: t
entry: a
nodez:
  a: {type: tool, tool: cargo_fmt}
"#;
        assert!(from_yaml(top).is_err(), "unknown top-level field accepted");

        let edge = r#"
name: t
entry: a
nodes:
  a: {type: tool, tool: cargo_fmt}
edges:
  - {form: a, to: END}
"#;
        assert!(from_yaml(edge).is_err(), "unknown edge field accepted");

        let node = r#"
name: t
entry: a
nodes:
  a: {type: agent, promt: "hi"}
"#;
        assert!(from_yaml(node).is_err(), "unknown node field accepted");
    }

    #[test]
    fn rejects_invalid_yaml() {
        let err = from_yaml("not: [a blueprint").err().unwrap().to_string();
        assert!(err.contains("parse blueprint YAML"), "{err}");
    }

    #[test]
    fn parses_container_nodes() {
        // Exercises the provision/checkout/exec variants, the `ref` field rename,
        // and the optional `path`/`workdir`/`ref` defaults.
        let src = r#"
name: c
entry: box
nodes:
  box:   {type: provision, image: "rust:latest"}
  clone: {type: checkout, container: "{box}", repo: "{task}", ref: main}
  test:  {type: exec, container: "{box}", run: "cargo test", workdir: /workspace}
edges:
  - {from: box, to: clone}
  - {from: clone, when: success, to: test}
  - {from: clone, when: failure, to: END}
  - {from: test, to: END}
"#;
        let graph = from_yaml(src).expect("valid container blueprint");
        assert_eq!(graph.route_for_test("box"), ["clone"]);
        assert_eq!(
            graph.route_with_status_for_test("clone", Some(Status::Ok)),
            ["test"]
        );
        assert_eq!(graph.route_for_test("test"), [END]);
    }

    #[test]
    fn parses_agent_sandbox_node() {
        // The `agent_sandbox` variant, with `port`/`command` defaulted and set.
        let src = r#"
name: c
entry: box
nodes:
  box:   {type: provision, image: "rust"}
  agent: {type: agent_sandbox, container: "{box}", prompt: "{task}"}
  other: {type: agent_sandbox, container: "{box}", prompt: "hi", port: 9100, command: "/usr/bin/boitata-agent"}
edges:
  - {from: box, to: agent}
  - {from: agent, to: other}
  - {from: other, to: END}
"#;
        let graph = from_yaml(src).expect("valid agent_sandbox blueprint");
        assert_eq!(graph.route_for_test("box"), ["agent"]);
        assert_eq!(graph.route_for_test("agent"), ["other"]);
        assert_eq!(graph.route_for_test("other"), [END]);
    }
}
