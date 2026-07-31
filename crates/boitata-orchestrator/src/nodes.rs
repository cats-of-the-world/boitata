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

use super::human::HumanInterface;
use super::state::{State, Status, Update, render, render_shell};
use boitata_agent::{Agent, Task};
use boitata_core::audit::{AuditSink, NodeKind};
use boitata_core::provider::Provider;
use boitata_core::tools::{ToolPolicy, ToolRegistry, run_script};

/// Shared, run-wide resources handed to every node.
pub struct NodeCtx<'a> {
    pub provider: Arc<dyn Provider>,
    pub tools: &'a ToolRegistry,
    pub audit: Option<Arc<dyn AuditSink>>,
    pub policy: &'a ToolPolicy,
    pub system_prompt: Option<&'a str>,
    pub max_iterations: Option<usize>,
    pub compact_threshold: Option<f32>,
    pub human: Arc<dyn HumanInterface>,
    pub cancel: CancellationToken,
}

impl<'a> NodeCtx<'a> {
    /// Clone this context but with a different cancellation token — used to hand
    /// the frontier nodes a per-super-step child token, so one node's failure can
    /// cancel its siblings without touching the run-wide token.
    pub fn with_cancel(&self, cancel: CancellationToken) -> NodeCtx<'a> {
        NodeCtx {
            provider: self.provider.clone(),
            tools: self.tools,
            audit: self.audit.clone(),
            policy: self.policy,
            system_prompt: self.system_prompt,
            max_iterations: self.max_iterations,
            compact_threshold: self.compact_threshold,
            human: self.human.clone(),
            cancel,
        }
    }
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
    /// Tool names this agent is restricted to; `None` means the full registry.
    tools: Option<Vec<String>>,
}

impl AgentNode {
    pub fn new(name: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            prompt: prompt.into(),
            tools: None,
        }
    }

    /// Restrict this agent to a subset of the registry's tools (the schema's
    /// per-node `tools:` list). Unknown names are rejected when the node runs.
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.tools = Some(tools);
        self
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

        // Scope the agent to its declared tools, if any; otherwise use the full
        // registry. An unknown tool name is a blueprint error surfaced here.
        let tools = match &self.tools {
            Some(names) => cx.tools.subset(names).map_err(|e| {
                anyhow::anyhow!("agent node `{}` lists an unknown tool: {e}", self.name)
            })?,
            None => cx.tools.clone(),
        };

        let mut agent = Agent::new(cx.provider.clone(), tools).with_policy(cx.policy.clone());
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

/// How a [`HumanNode`] interprets the operator's reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanMode {
    /// Free-text: the reply text becomes the node's output; status is always Ok.
    #[default]
    Input,
    /// A yes/no gate: an affirmative reply is `Ok`, anything else is `Failed`, so
    /// a conditional edge can branch (`when: success` to proceed, `when: failure`
    /// to abort).
    Approval,
}

/// A node that pauses the run to collect input from a human operator, via the
/// [`HumanInterface`] on the context (human-in-the-loop). The prompt may template
/// `{task}` and `{<var>}` from state.
pub struct HumanNode {
    name: String,
    prompt: String,
    mode: HumanMode,
}

impl HumanNode {
    pub fn new(name: impl Into<String>, prompt: impl Into<String>, mode: HumanMode) -> Self {
        Self {
            name: name.into(),
            prompt: prompt.into(),
            mode,
        }
    }
}

/// Whether an approval reply is affirmative: `y`, `yes`, or `ok` (any case). An
/// empty reply (a bare Enter) is deliberately *not* affirmative, so approval
/// defaults to "no".
fn is_affirmative(reply: &str) -> bool {
    matches!(
        reply.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "ok"
    )
}

#[async_trait]
impl Node for HumanNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Human
    }

    /// Presents the prompt and collects a reply. A missing input (non-interactive
    /// stdin) or a cancellation is a hard error (propagated), not a `Failed`
    /// status, so it can't be mistaken for a decline.
    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let prompt = render(&self.prompt, state);
        let reply = cx.human.prompt(&prompt, &cx.cancel).await?;
        let update = match self.mode {
            HumanMode::Input => Update::from_node(&self.name, reply, Status::Ok),
            HumanMode::Approval => {
                if is_affirmative(&reply) {
                    Update::from_node(&self.name, "approved".to_string(), Status::Ok)
                } else {
                    Update::from_node(&self.name, "declined".to_string(), Status::Failed)
                }
            }
        };
        Ok(update)
    }
}
