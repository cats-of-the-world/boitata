// Built-in blueprints, defined in code and selected by name via `--blueprint`.
// A YAML loader and a fuller library are a later phase (see docs/blueprint.md).

use serde_json::json;

use super::nodes::{AgentNode, ScriptNode, ToolNode};
use super::state::Status;
use super::{END, Graph, GraphBuilder};

/// The names of every built-in blueprint, for help/error messages. Keep in sync
/// with [`by_name`].
pub const KNOWN: &[&str] = &["default"];

/// Look up a built-in blueprint by name.
pub fn by_name(name: &str) -> Option<Graph> {
    let builder = match name {
        "default" => default_blueprint(),
        _ => return None,
    };
    // The built-in blueprints are static and valid; a bug here is a programmer
    // error, not user input.
    Some(builder.build().expect("built-in blueprint is valid"))
}

/// The default blueprint, exercising all three node kinds: the agent does the
/// task, `cargo fmt` runs before finishing, and `cargo check` verifies; a failed
/// check loops back to the agent.
///
///   main (agent) -> fmt (tool) -> verify (script)
///   verify failed -> main;  verify ok -> END
fn default_blueprint() -> GraphBuilder {
    Graph::builder("default", "main")
        .node(AgentNode::new("main", "{task}"))
        .node(ToolNode::new("fmt", "cargo_fmt", json!({})))
        .node(ScriptNode::new("verify", "cargo check"))
        .edge("main", "fmt")
        .edge("fmt", "verify")
        .conditional("verify", |state| match state.status {
            Some(Status::Failed) => "main".to_string(),
            _ => END.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_blueprint_builds() {
        assert!(by_name("default").is_some());
        assert!(by_name("nope").is_none());
    }
}
