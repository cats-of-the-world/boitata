// Blueprint nodes: the three kinds of step a blueprint can run.
//
// - AgentNode  runs the LLM agent loop (`Agent`) on a prompt.
// - ToolNode   invokes a named registered tool with fixed arguments.
// - ScriptNode runs a shell script deterministically (e.g. set up a devbox).
//
// Each node reads the current [`State`] and returns an [`Update`]; the executor
// merges it. Nodes receive shared run resources through [`NodeCtx`].

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::state::{State, Status, Update, render};
use crate::agent::{Agent, Task};
use crate::audit::AuditSink;
use crate::provider::Provider;
use crate::tools::{ToolPolicy, ToolRegistry, run_script};

/// Shared, run-wide resources handed to every node.
pub struct NodeCtx<'a> {
    pub provider: Arc<dyn Provider>,
    pub tools: &'a ToolRegistry,
    pub audit: Option<Arc<dyn AuditSink>>,
    pub policy: &'a ToolPolicy,
    pub system_prompt: Option<&'a str>,
    pub max_iterations: Option<usize>,
    pub compact_threshold: Option<f32>,
    pub cancel: CancellationToken,
}

/// A single step in a blueprint graph.
#[async_trait]
pub trait Node: Send + Sync {
    /// Unique id of this node within its graph.
    fn name(&self) -> &str;

    /// Node kind label ("agent" | "tool" | "script"), for audit/logging.
    fn kind(&self) -> &'static str;

    /// Execute the node against the current state, returning a state update.
    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update>;
}

/// A node that runs the LLM agent loop on a prompt (which may template `{task}`
/// and `{<var>}` from state).
pub struct AgentNode {
    name: String,
    prompt: String,
}

impl AgentNode {
    pub fn new(name: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            prompt: prompt.into(),
        }
    }
}

#[async_trait]
impl Node for AgentNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "agent"
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let prompt = render(&self.prompt, state);

        let mut agent =
            Agent::new(cx.provider.clone(), cx.tools.clone()).with_policy(cx.policy.clone());
        if let Some(system) = cx.system_prompt {
            agent = agent.with_system_prompt(system.to_string());
        }
        if let Some(max) = cx.max_iterations {
            agent = agent.with_max_iterations(max);
        }
        if let Some(threshold) = cx.compact_threshold {
            agent = agent.with_compact_threshold(threshold);
        }
        if let Some(audit) = &cx.audit {
            agent = agent.with_audit(audit.clone());
        }

        let result = agent
            .run_with_cancel(Task::new(prompt), cx.cancel.clone())
            .await?;

        let text = result
            .final_message
            .or(result.error)
            .unwrap_or_else(|| "(no output)".to_string());
        let status = if result.success {
            Status::Ok
        } else {
            Status::Failed
        };
        Ok(Update::from_node(&self.name, text, status))
    }
}

/// A node that invokes a named registered tool with fixed arguments.
pub struct ToolNode {
    name: String,
    tool: String,
    args: Value,
}

impl ToolNode {
    pub fn new(name: impl Into<String>, tool: impl Into<String>, args: Value) -> Self {
        Self {
            name: name.into(),
            tool: tool.into(),
            args,
        }
    }
}

#[async_trait]
impl Node for ToolNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "tool"
    }

    async fn run(&self, _state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        match cx
            .tools
            .execute(&self.tool, &self.args, cx.cancel.clone())
            .await
        {
            Ok(output) => {
                let text = output.to_text();
                // Command-backed tools (cargo_*, git_*, execute_command) report a
                // non-zero exit in their output text rather than as an error, so
                // treat that as a failure for routing.
                let status = status_from_output(&text);
                Ok(Update::from_node(&self.name, text, status))
            }
            Err(e) => Ok(Update::from_node(
                &self.name,
                format!("error: {e}"),
                Status::Failed,
            )),
        }
    }
}

/// A node that runs a shell script deterministically. Routes on the exit code.
pub struct ScriptNode {
    name: String,
    script: String,
}

impl ScriptNode {
    pub fn new(name: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            script: script.into(),
        }
    }
}

#[async_trait]
impl Node for ScriptNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "script"
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let script = render(&self.script, state);
        let result = run_script(&script, None, &cx.cancel).await?;
        let status = if result.code == Some(0) {
            Status::Ok
        } else {
            Status::Failed
        };
        Ok(Update::from_node(&self.name, result.output, status))
    }
}

/// Derive a status from a command-backed tool's output text. The exec layer
/// prefixes non-zero exits with `[exit code N]` and signal deaths with
/// `[terminated by signal]`; anything else is treated as success.
fn status_from_output(text: &str) -> Status {
    if text.starts_with("[exit code ") || text.starts_with("[terminated by signal]") {
        Status::Failed
    } else {
        Status::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_exit_marker() {
        assert_eq!(status_from_output("[exit code 101]\nerror"), Status::Failed);
        assert_eq!(status_from_output("[terminated by signal]"), Status::Failed);
        assert_eq!(status_from_output("all good"), Status::Ok);
        assert_eq!(status_from_output(""), Status::Ok);
    }
}
