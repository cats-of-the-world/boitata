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
use serde::Deserialize;
use serde_json::Value;

use super::nodes::{AgentNode, ScriptNode, ToolNode};
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
}

/// Tool args default to an empty object rather than `null`, since tools expect
/// an object of arguments.
fn empty_args() -> Value {
    Value::Object(serde_json::Map::new())
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
            // No `when`: exactly one unconditional edge is allowed.
            if group.len() != 1 {
                bail!(
                    "node `{from}` has multiple unconditional edges; use `when: success|failure` to branch"
                );
            }
            let to = normalize_target(&group[0].to);
            builder = builder.edge(from, to);
        }
    }
    Ok(builder)
}

/// Fold a group of `when`-tagged edges out of `from` into one router that maps
/// the last node's status to a target (falling back to [`END`]).
fn add_conditional(
    builder: GraphBuilder,
    from: &str,
    group: Vec<EdgeDef>,
    known: &HashSet<String>,
) -> anyhow::Result<GraphBuilder> {
    let mut on_success: Option<String> = None;
    let mut on_failure: Option<String> = None;
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
        let slot = match when {
            "success" => &mut on_success,
            "failure" => &mut on_failure,
            other => bail!("edge from `{from}` has unknown `when: {other}` (use success|failure)"),
        };
        if slot.replace(target).is_some() {
            bail!("node `{from}` has two `when: {when}` edges");
        }
    }

    // Routes not covered by an edge fall through to END. A missing node status
    // (only before any node has run) is treated as success.
    Ok(builder.conditional(from.to_string(), move |state| {
        let target = match state.status {
            Some(Status::Failed) => on_failure.as_deref(),
            _ => on_success.as_deref(),
        };
        target.unwrap_or(END).to_string()
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
    fn parses_and_compiles_a_full_blueprint() {
        let graph = from_yaml(SAMPLE).expect("valid blueprint");
        // Static edges resolve to their targets; the conditional router branches
        // on status: failure -> fix, success -> END.
        assert_eq!(graph.route_for_test("fix"), "fmt");
        assert_eq!(graph.route_for_test("fmt"), "verify");
        assert_eq!(
            graph.route_with_status_for_test("verify", Some(Status::Failed)),
            "fix"
        );
        assert_eq!(
            graph.route_with_status_for_test("verify", Some(Status::Ok)),
            END
        );
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
        assert_eq!(graph.route_for_test("a"), END);
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
        assert_eq!(graph.route_for_test("a"), END);
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
    fn rejects_duplicate_when_branch() {
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
  b: {type: script, run: "true"}
edges:
  - {from: a, when: failure, to: a}
  - {from: a, when: failure, to: b}
"#;
        let err = from_yaml(src).err().unwrap().to_string();
        assert!(err.contains("two `when: failure`"), "{err}");
    }

    #[test]
    fn rejects_multiple_unconditional_edges() {
        let src = r#"
name: t
entry: a
nodes:
  a: {type: script, run: "true"}
  b: {type: script, run: "true"}
edges:
  - {from: a, to: b}
  - {from: a, to: END}
"#;
        let err = from_yaml(src).err().unwrap().to_string();
        assert!(err.contains("multiple unconditional edges"), "{err}");
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
}
