// Blueprint state: the typed channels threaded through a graph run.
//
// Nodes never mutate the state directly. A node reads the current state and
// returns an [`Update`] (a set of channel writes); the executor merges each
// write through that channel's reducer (see [`State::apply`]). This mirrors
// LangGraph's isolated-state model.

use std::collections::HashMap;

/// Outcome of the most recently executed node, used for conditional routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
}

impl Status {
    /// Short label for audit/logging.
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
        }
    }
}

/// One entry in the running transcript: which node produced it and the text.
#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    pub node: String,
    pub text: String,
}

/// The shared state of a blueprint run.
#[derive(Debug, Clone)]
pub struct State {
    /// The original task. Set once at construction; nodes never change it.
    pub task: String,
    /// Accumulated transcript of node outputs. Channel reducer: append.
    pub messages: Vec<TranscriptEntry>,
    /// Outcome of the last node. Channel reducer: last-write.
    pub status: Option<Status>,
    /// Free-form values nodes emit for routing and prompt/arg templating.
    /// Channel reducer: merge.
    pub vars: HashMap<String, String>,
}

impl State {
    pub fn new(task: String) -> Self {
        Self {
            task,
            messages: Vec::new(),
            status: None,
            vars: HashMap::new(),
        }
    }

    /// Iterate the transcript as `(node, text)` pairs, for reporting without
    /// exposing the entry type.
    pub fn transcript(&self) -> impl Iterator<Item = (&str, &str)> {
        self.messages
            .iter()
            .map(|entry| (entry.node.as_str(), entry.text.as_str()))
    }

    /// Merge a node's update into the state through the per-channel reducers.
    pub fn apply(&mut self, update: Update) {
        // messages: append
        self.messages.extend(update.messages);
        // status: last-write (only when the node reported one)
        if update.status.is_some() {
            self.status = update.status;
        }
        // vars: merge (later writes win)
        self.vars.extend(update.vars);
        // task: set-once — updates never touch it
    }
}

/// A node's output: writes that the executor merges into [`State`].
#[derive(Debug, Clone, Default)]
pub struct Update {
    pub messages: Vec<TranscriptEntry>,
    pub status: Option<Status>,
    pub vars: HashMap<String, String>,
}

impl Update {
    /// Build the common update: one transcript entry from `node`, a status, and
    /// a `vars` entry keyed by the node name holding the same text (so later
    /// nodes can template `{node}`).
    pub fn from_node(node: &str, text: String, status: Status) -> Self {
        let mut vars = HashMap::new();
        vars.insert(node.to_string(), text.clone());
        Self {
            messages: vec![TranscriptEntry {
                node: node.to_string(),
                text,
            }],
            status: Some(status),
            vars,
        }
    }
}

/// Substitute `{task}` and `{<var>}` placeholders in `template` from `state`.
/// Unknown placeholders are left untouched.
pub fn render(template: &str, state: &State) -> String {
    let mut out = template.replace("{task}", &state.task);
    for (key, value) in &state.vars {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_reduces_each_channel() {
        let mut state = State::new("do it".to_string());
        state.apply(Update::from_node("a", "first".to_string(), Status::Ok));
        state.apply(Update::from_node("b", "second".to_string(), Status::Failed));

        // messages appended, status last-write, vars merged, task untouched.
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.status, Some(Status::Failed));
        assert_eq!(state.vars.get("a").map(String::as_str), Some("first"));
        assert_eq!(state.vars.get("b").map(String::as_str), Some("second"));
        assert_eq!(state.task, "do it");
    }

    #[test]
    fn render_substitutes_task_and_vars() {
        let mut state = State::new("fix the bug".to_string());
        state
            .vars
            .insert("verify".to_string(), "exit code 1".to_string());
        let rendered = render(
            "Task: {task}. Last check: {verify}. Unknown: {nope}",
            &state,
        );
        assert_eq!(
            rendered,
            "Task: fix the bug. Last check: exit code 1. Unknown: {nope}"
        );
    }
}
