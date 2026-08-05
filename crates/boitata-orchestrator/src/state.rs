// Blueprint state: the typed channels threaded through a graph run.
//
// Nodes never mutate the state directly. A node reads the current state and
// returns an [`Update`] (a set of channel writes); the executor merges each
// write through that channel's reducer (see [`State::apply`]). This mirrors
// LangGraph's isolated-state model.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Outcome of the most recently executed node, used for conditional routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Failed,
}

/// One entry in the running transcript: which node produced it and the text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub node: String,
    pub text: String,
}

/// The shared state of a blueprint run.
///
/// `Serialize`/`Deserialize` let the executor persist a run's state to a
/// [`Checkpointer`](crate::Checkpointer) between super-steps and restore it to
/// resume — so a cancelled or crashed run picks up from the last super-step
/// instead of starting over.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// The original task. Set once at construction; nodes never change it.
    pub task: String,
    /// Accumulated transcript of node outputs. Channel reducer: append.
    pub messages: Vec<TranscriptEntry>,
    /// Outcome of the last node. Channel reducer: last-write.
    pub status: Option<Status>,
    /// Free-form values nodes emit for routing and prompt/arg templating.
    /// Channel reducer: merge, last-write-wins per key. When a super-step fans
    /// out, updates merge in sorted node-name order, so two parallel nodes
    /// writing the same key resolve deterministically (by name); in practice each
    /// node writes only its own unique name key.
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
/// Unknown placeholders are left untouched. Used for LLM prompts (values are not
/// escaped).
///
/// Single pass over the template: substituted values are never re-scanned, so a
/// value that itself contains `{...}` cannot trigger further substitution and the
/// result does not depend on `vars` iteration order.
pub fn render(template: &str, state: &State) -> String {
    render_with(template, state, |value| value.to_string())
}

/// Like [`render`], but shell-escapes each substituted value (wraps it in single
/// quotes) so interpolating untrusted state into a `sh -c` script cannot inject
/// commands. Used by [`super::nodes::ScriptNode`].
pub fn render_shell(template: &str, state: &State) -> String {
    render_with(template, state, shell_single_quote)
}

fn render_with(template: &str, state: &State, escape: impl Fn(&str) -> String) -> String {
    let resolve = |key: &str| -> Option<&str> {
        if key == "task" {
            Some(state.task.as_str())
        } else {
            state.vars.get(key).map(String::as_str)
        }
    };

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        match rest[open..].find('}') {
            Some(close) => {
                let key = &rest[open + 1..open + close];
                match resolve(key) {
                    Some(value) => out.push_str(&escape(value)),
                    // Unknown placeholder: keep it verbatim.
                    None => out.push_str(&rest[open..open + close + 1]),
                }
                rest = &rest[open + close + 1..];
            }
            // No closing brace: emit the remainder (from the brace) as-is.
            None => {
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Single-quote a value for safe inclusion in a `sh -c` command line: wrap in
/// `'...'` and rewrite each embedded `'` as `'\''`. The result is a single shell
/// word with no metacharacter interpretation.
fn shell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
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

    #[test]
    fn render_does_not_rescan_substituted_values() {
        // A value that itself looks like a placeholder must not be re-substituted,
        // and the result must not depend on vars iteration order.
        let mut state = State::new("t".to_string());
        state.vars.insert("a".to_string(), "{b}".to_string());
        state.vars.insert("b".to_string(), "SECRET".to_string());
        assert_eq!(render("{a}", &state), "{b}");
    }

    #[test]
    fn render_handles_unclosed_brace() {
        let state = State::new("t".to_string());
        assert_eq!(render("a {b", &state), "a {b");
    }

    #[test]
    fn render_shell_escapes_interpolated_values() {
        let mut state = State::new("t".to_string());
        state.vars.insert("x".to_string(), "; rm -rf /".to_string());
        // Metacharacters are neutralized inside single quotes.
        assert_eq!(render_shell("echo {x}", &state), "echo '; rm -rf /'");

        // Embedded single quote is closed, escaped, and reopened.
        state.vars.insert("y".to_string(), "a'b".to_string());
        assert_eq!(render_shell("echo {y}", &state), r"echo 'a'\''b'");

        // Literal template text is untouched; only values are quoted.
        assert_eq!(render_shell("ls {nope}", &state), "ls {nope}");
    }
}
