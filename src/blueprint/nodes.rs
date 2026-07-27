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

use super::state::{State, Status, Update, render, render_shell};
use crate::agent::{Agent, Task};
use crate::audit::{AuditSink, NodeKind};
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

    /// Node kind, for audit/logging.
    fn kind(&self) -> NodeKind;

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

    fn kind(&self) -> NodeKind {
        NodeKind::Agent
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

        let status = if result.success {
            Status::Ok
        } else {
            Status::Failed
        };
        let text = result.final_message.or(result.error).unwrap_or_else(|| {
            if result.success {
                format!("agent `{}` produced no output", self.name)
            } else {
                format!("agent `{}` failed with no error message", self.name)
            }
        });
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

    fn kind(&self) -> NodeKind {
        NodeKind::Tool
    }

    /// Status reflects whether the tool *executed*, not the exit code of any
    /// command it wrapped: command-backed tools (cargo_*, git_*) report a failing
    /// command as normal output text rather than an error, so this node reports
    /// `Ok` for them. Use a [`ScriptNode`] when routing must depend on a command's
    /// exit code.
    async fn run(&self, _state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        match cx
            .tools
            .execute(&self.tool, &self.args, cx.cancel.clone())
            .await
        {
            Ok(output) => Ok(Update::from_node(&self.name, output.to_text(), Status::Ok)),
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

    fn kind(&self) -> NodeKind {
        NodeKind::Script
    }

    /// The script is run via `sh -c` after templating with `{task}` and
    /// `{<var>}`. Interpolated values are shell-escaped (single-quoted, see
    /// [`render_shell`]) so a node output containing shell metacharacters is
    /// treated as literal text rather than injected as commands.
    ///
    /// A launch/timeout/cancellation error is reported as `Status::Failed`
    /// (uniform with [`ToolNode`]); the executor's post-node cancellation check
    /// still stops the run promptly on Ctrl-C.
    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let script = render_shell(&self.script, state);
        match run_script(&script, None, &cx.cancel).await {
            Ok(result) => {
                let status = if result.code == Some(0) {
                    Status::Ok
                } else {
                    Status::Failed
                };
                Ok(Update::from_node(&self.name, result.output, status))
            }
            Err(e) => Ok(Update::from_node(
                &self.name,
                format!("error: {e}"),
                Status::Failed,
            )),
        }
    }
}
