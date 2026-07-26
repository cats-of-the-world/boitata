// Blueprint: a graph of nodes (agents, tools, or scripts) executed over a typed
// state, with static and conditional edges and cycles. This is the hybrid
// deterministic/agentic workflow engine (Sprint 4); see docs/blueprint.md.
//
// The executor is sequential: one node runs per step, its update is merged into
// the state, then routing picks the next node, until END or a step limit. It
// reuses the agent loop, the tool registry, and the shell-exec infrastructure.

mod library;
mod nodes;
mod state;

pub use library::by_name;
pub use state::{State, Status};

use nodes::{Node, NodeCtx};
use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::audit::{AuditEvent, AuditSink, CompletionReason, NodeStatus};
use crate::provider::Provider;
use crate::tools::{ToolPolicy, ToolRegistry};

/// Sentinel target that ends a blueprint run.
pub const END: &str = "__end__";

/// Default cap on executed nodes, so a cyclic graph can't loop forever.
const DEFAULT_MAX_STEPS: usize = 50;

/// How the run proceeds after a node finishes.
type Router = Box<dyn Fn(&State) -> String + Send + Sync>;

enum Routing {
    /// Always go to a fixed node (or END).
    Static(String),
    /// Choose the next node (or END) from the state.
    Conditional(Router),
}

#[derive(Debug, Error)]
pub enum BlueprintError {
    #[error("blueprint graph is invalid: {0}")]
    Invalid(String),
    #[error("blueprint routed to unknown node `{0}`")]
    UnknownNode(String),
    #[error("blueprint exceeded its step limit ({0})")]
    StepLimit(usize),
}

/// A compiled blueprint: nodes plus the routing out of each.
pub struct Graph {
    name: String,
    entry: String,
    nodes: HashMap<String, Box<dyn Node>>,
    edges: HashMap<String, Routing>,
}

impl Graph {
    pub fn builder(name: impl Into<String>, entry: impl Into<String>) -> GraphBuilder {
        GraphBuilder {
            name: name.into(),
            entry: entry.into(),
            nodes: HashMap::new(),
            edges: HashMap::new(),
            duplicates: Vec::new(),
        }
    }
}

/// Builder for a [`Graph`], validated on [`GraphBuilder::build`].
pub struct GraphBuilder {
    name: String,
    entry: String,
    nodes: HashMap<String, Box<dyn Node>>,
    edges: HashMap<String, Routing>,
    /// Node names that were added more than once; rejected by `build`.
    duplicates: Vec<String>,
}

impl GraphBuilder {
    /// Add a node, keyed by its own name. Adding two nodes with the same name is
    /// a mistake that `build` rejects (rather than silently dropping one).
    pub fn node(mut self, node: impl Node + 'static) -> Self {
        let name = node.name().to_string();
        if self.nodes.insert(name.clone(), Box::new(node)).is_some() {
            self.duplicates.push(name);
        }
        self
    }

    /// Add an unconditional edge `from -> to` (`to` may be [`END`]).
    pub fn edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.edges.insert(from.into(), Routing::Static(to.into()));
        self
    }

    /// Add a conditional edge: after `from`, `router(state)` picks the next node
    /// (or [`END`]).
    pub fn conditional(
        mut self,
        from: impl Into<String>,
        router: impl Fn(&State) -> String + Send + Sync + 'static,
    ) -> Self {
        self.edges
            .insert(from.into(), Routing::Conditional(Box::new(router)));
        self
    }

    /// Validate and finalize the graph. Checks that the entry and all static
    /// edge endpoints refer to known nodes (or END). Conditional targets are
    /// checked at run time.
    pub fn build(self) -> Result<Graph, BlueprintError> {
        let known = |id: &str| id == END || self.nodes.contains_key(id);

        if !self.duplicates.is_empty() {
            return Err(BlueprintError::Invalid(format!(
                "duplicate node name(s): {}",
                self.duplicates.join(", ")
            )));
        }
        if !self.nodes.contains_key(&self.entry) {
            return Err(BlueprintError::Invalid(format!(
                "entry `{}` is not a node",
                self.entry
            )));
        }
        for (from, routing) in &self.edges {
            if !self.nodes.contains_key(from) {
                return Err(BlueprintError::Invalid(format!(
                    "edge source `{from}` is not a node"
                )));
            }
            if let Routing::Static(to) = routing {
                if !known(to) {
                    return Err(BlueprintError::Invalid(format!(
                        "edge `{from}` -> `{to}` targets an unknown node"
                    )));
                }
            }
        }
        Ok(Graph {
            name: self.name,
            entry: self.entry,
            nodes: self.nodes,
            edges: self.edges,
        })
    }
}

/// Runs blueprints. Carries the shared resources every node needs and the
/// agent-node configuration (system prompt, iteration/compaction limits).
pub struct Executor {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    audit: Option<Arc<dyn AuditSink>>,
    policy: ToolPolicy,
    system_prompt: Option<String>,
    max_iterations: Option<usize>,
    compact_threshold: Option<f32>,
    max_steps: usize,
}

impl Executor {
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Self {
        Self {
            provider,
            tools,
            audit: None,
            policy: ToolPolicy::allow_all(),
            system_prompt: None,
            max_iterations: None,
            compact_threshold: None,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn with_policy(mut self, policy: ToolPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_system_prompt(mut self, prompt: Option<String>) -> Self {
        self.system_prompt = prompt;
        self
    }

    pub fn with_max_iterations(mut self, max: Option<usize>) -> Self {
        self.max_iterations = max;
        self
    }

    pub fn with_compact_threshold(mut self, threshold: Option<f32>) -> Self {
        self.compact_threshold = threshold;
        self
    }

    fn emit(&self, event: AuditEvent) {
        if let Some(audit) = &self.audit {
            audit.record(event);
        }
    }

    /// Run `graph` on `task`, installing a Ctrl-C watcher that cancels the run.
    pub async fn run(&self, graph: &Graph, task: String) -> anyhow::Result<State> {
        let cancel = CancellationToken::new();
        let watcher = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("Interrupt received; cancelling blueprint");
                    cancel.cancel();
                }
            })
        };
        let result = self.run_with_cancel(graph, task, cancel).await;
        watcher.abort();
        result
    }

    /// Run `graph` under an external cancellation token.
    pub async fn run_with_cancel(
        &self,
        graph: &Graph,
        task: String,
        cancel: CancellationToken,
    ) -> anyhow::Result<State> {
        info!("Starting blueprint `{}` on task: {task}", graph.name);
        self.emit(AuditEvent::BlueprintStarted {
            blueprint: graph.name.clone(),
            entry: graph.entry.clone(),
        });

        let mut state = State::new(task);
        let cx = NodeCtx {
            provider: self.provider.clone(),
            tools: &self.tools,
            audit: self.audit.clone(),
            policy: &self.policy,
            system_prompt: self.system_prompt.as_deref(),
            max_iterations: self.max_iterations,
            compact_threshold: self.compact_threshold,
            cancel: cancel.clone(),
        };

        // `steps` in the completion events is the number of nodes executed. The
        // normal path reaches END at the top of iteration `step` having run `step`
        // nodes; the cancelled path is mid-iteration having run `step + 1`; the
        // step-limit path has run `max_steps`. All three are that same count.
        let mut current = graph.entry.clone();
        for step in 0..self.max_steps {
            if current == END {
                self.emit(AuditEvent::BlueprintCompleted {
                    steps: step,
                    reason: CompletionReason::Completed,
                });
                return Ok(state);
            }

            let node = graph
                .nodes
                .get(&current)
                .ok_or_else(|| BlueprintError::UnknownNode(current.clone()))?;
            let kind = node.kind();
            debug!("Blueprint step {}: node `{current}` ({kind:?})", step + 1);

            let update = match node.run(&state, &cx).await {
                Ok(update) => update,
                Err(e) => {
                    self.emit(AuditEvent::BlueprintCompleted {
                        steps: step + 1,
                        reason: CompletionReason::Error,
                    });
                    return Err(e.context(format!("blueprint node `{current}` failed")));
                }
            };
            state.apply(update);
            let status = match state.status {
                Some(Status::Failed) => NodeStatus::Failed,
                _ => NodeStatus::Ok,
            };

            // Stop if cancelled while the node was running.
            if cancel.is_cancelled() {
                warn!("Blueprint cancelled at node `{current}`");
                self.emit(AuditEvent::BlueprintCompleted {
                    steps: step + 1,
                    reason: CompletionReason::Cancelled,
                });
                return Ok(state);
            }

            let next = match Self::route(graph, &current, &state) {
                Ok(next) => next,
                Err(e) => {
                    self.emit(AuditEvent::BlueprintCompleted {
                        steps: step + 1,
                        reason: CompletionReason::Error,
                    });
                    return Err(e.context(format!("routing after node `{current}`")));
                }
            };
            self.emit(AuditEvent::NodeExecuted {
                step: step + 1,
                node: current.clone(),
                kind,
                status,
                next: next.clone(),
            });
            current = next;
        }

        warn!(
            "Blueprint `{}` hit the step limit ({})",
            graph.name, self.max_steps
        );
        self.emit(AuditEvent::BlueprintCompleted {
            steps: self.max_steps,
            reason: CompletionReason::StepLimit,
        });
        Err(BlueprintError::StepLimit(self.max_steps).into())
    }

    /// Pick the next node after `current`. A node with no outgoing edge ends the
    /// run.
    fn route(graph: &Graph, current: &str, state: &State) -> anyhow::Result<String> {
        let next = match graph.edges.get(current) {
            Some(Routing::Static(to)) => to.clone(),
            Some(Routing::Conditional(router)) => router(state),
            None => END.to_string(),
        };
        if next != END && !graph.nodes.contains_key(&next) {
            return Err(BlueprintError::UnknownNode(next).into());
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::state::Update;
    use super::*;
    use crate::audit::NodeKind;
    use crate::provider::{Chunk, CompletionRequest, CompletionResponse, ProviderResult};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Minimal provider: never actually used because the test graphs use tool and
    // script nodes only.
    struct DummyProvider;

    #[async_trait]
    impl Provider for DummyProvider {
        fn name(&self) -> &str {
            "dummy"
        }
        fn model(&self) -> &str {
            "dummy"
        }
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> ProviderResult<CompletionResponse> {
            Ok(CompletionResponse {
                content: Some("ok".to_string()),
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: Some("stop".to_string()),
            })
        }
        async fn stream_complete(
            &self,
            _request: CompletionRequest,
        ) -> ProviderResult<tokio_stream::wrappers::ReceiverStream<ProviderResult<Chunk>>> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
        }
    }

    // A node that records how many times it ran and reports a preset status.
    struct CountingNode {
        name: String,
        status: Status,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Node for CountingNode {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> NodeKind {
            NodeKind::Tool
        }
        async fn run(&self, _state: &State, _cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(Update::from_node(&self.name, "x".to_string(), self.status))
        }
    }

    fn executor() -> Executor {
        Executor::new(Arc::new(DummyProvider), ToolRegistry::new())
    }

    #[tokio::test]
    async fn runs_nodes_in_order_to_end() {
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let graph = Graph::builder("t", "a")
            .node(CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs: a.clone(),
            })
            .node(CountingNode {
                name: "b".into(),
                status: Status::Ok,
                runs: b.clone(),
            })
            .edge("a", "b")
            .edge("b", END)
            .build()
            .unwrap();

        let state = executor().run(&graph, "task".into()).await.unwrap();
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
        assert_eq!(state.messages.len(), 2);
    }

    #[tokio::test]
    async fn conditional_edge_loops_then_exits() {
        // `check` fails the first two times, then succeeds; failure loops back to
        // `work`, success ends the run.
        let work = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));

        struct FlakyCheck {
            attempts: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Node for FlakyCheck {
            fn name(&self) -> &str {
                "check"
            }
            fn kind(&self) -> NodeKind {
                NodeKind::Tool
            }
            async fn run(&self, _s: &State, _c: &NodeCtx<'_>) -> anyhow::Result<Update> {
                let n = self.attempts.fetch_add(1, Ordering::SeqCst);
                let status = if n < 2 { Status::Failed } else { Status::Ok };
                Ok(Update::from_node("check", "x".into(), status))
            }
        }

        let graph = Graph::builder("loop", "work")
            .node(CountingNode {
                name: "work".into(),
                status: Status::Ok,
                runs: work.clone(),
            })
            .node(FlakyCheck {
                attempts: attempts.clone(),
            })
            .edge("work", "check")
            .conditional("check", |state| match state.status {
                Some(Status::Failed) => "work".to_string(),
                _ => END.to_string(),
            })
            .build()
            .unwrap();

        let state = executor().run(&graph, "task".into()).await.unwrap();
        assert_eq!(work.load(Ordering::SeqCst), 3);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(state.status, Some(Status::Ok));
    }

    #[tokio::test]
    async fn step_limit_is_enforced() {
        // An unconditional self-loop must stop at the step limit.
        let runs = Arc::new(AtomicUsize::new(0));
        let graph = Graph::builder("spin", "a")
            .node(CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs: runs.clone(),
            })
            .edge("a", "a")
            .build()
            .unwrap();

        let mut exec = executor();
        exec.max_steps = 5;
        let err = exec.run(&graph, "task".into()).await.unwrap_err();
        assert!(err.to_string().contains("step limit"));
        assert_eq!(runs.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn script_node_runs_and_routes_on_exit_code() {
        // A real script node: `setup` succeeds, `check` exits non-zero. The
        // failing check routes to END (rather than looping), and its status and
        // output land in the state.
        use super::nodes::ScriptNode;

        let graph = Graph::builder("scripted", "setup")
            .node(ScriptNode::new("setup", "echo preparing"))
            .node(ScriptNode::new("check", "exit 3"))
            .edge("setup", "check")
            .conditional("check", |_state| END.to_string())
            .build()
            .unwrap();

        let state = executor().run(&graph, "task".into()).await.unwrap();
        assert_eq!(state.status, Some(Status::Failed));
        assert_eq!(
            state.vars.get("setup").map(String::as_str),
            Some("preparing")
        );
        assert!(
            state
                .vars
                .get("check")
                .is_some_and(|v| v.contains("exit code 3")),
            "check output should note the exit code: {:?}",
            state.vars.get("check")
        );
    }

    #[test]
    fn build_rejects_unknown_entry_and_targets() {
        let runs = Arc::new(AtomicUsize::new(0));
        assert!(
            Graph::builder("g", "missing")
                .node(CountingNode {
                    name: "a".into(),
                    status: Status::Ok,
                    runs: runs.clone(),
                })
                .build()
                .is_err()
        );
        assert!(
            Graph::builder("g", "a")
                .node(CountingNode {
                    name: "a".into(),
                    status: Status::Ok,
                    runs,
                })
                .edge("a", "nope")
                .build()
                .is_err()
        );
    }

    #[test]
    fn build_rejects_duplicate_node_names() {
        let runs = Arc::new(AtomicUsize::new(0));
        let result = Graph::builder("g", "a")
            .node(CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs: runs.clone(),
            })
            .node(CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs,
            })
            .build();
        assert!(
            matches!(result, Err(BlueprintError::Invalid(msg)) if msg.contains("duplicate node"))
        );
    }
}
