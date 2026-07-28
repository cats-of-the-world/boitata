// Blueprint: a hybrid deterministic/agentic workflow engine. A blueprint is a
// graph of nodes run over a typed state, from an entry node until END, with
// static and conditional edges and cycles. The design follows LangGraph, adapted
// so a node can be an agent, a tool, or a script.
//
// State (see `state.rs`): named channels, each with a reducer. A node never
// mutates the state; it reads the current state and returns an `Update` (a set of
// channel writes) that the executor merges through each channel's reducer:
//   - messages: transcript of node outputs (reducer: append)
//   - task:     the original task string       (reducer: set-once)
//   - status:   the last node's outcome         (reducer: last-write)
//   - vars:     values nodes emit for routing and `{task}`/`{var}` templating
//               (reducer: merge)
//
// Nodes (see `nodes.rs`), three kinds:
//   - agent:  runs the LLM agent loop (`Agent`) on a prompt
//   - tool:   invokes a named tool from the `ToolRegistry`
//   - script: runs a shell script (reusing the exec infrastructure), routing on
//             its exit code
//
// Edges: `Static(from -> to)` or `Conditional(from -> router(state) -> next)`,
// where a target may be the `END` sentinel. A node with no outgoing edge ends the
// run.
//
// The executor is sequential: one node runs per step, its update is merged into
// the state, then routing picks the next node, until END or a step limit
// (`max_steps`, which bounds cyclic graphs). Ctrl-C cancels the run, and each
// step emits audit events (see `crate::audit`).
//
// Blueprints are defined in YAML (see `yaml.rs`). A small starter library ships
// embedded in the binary, and `--blueprint` also accepts a path to a user's own
// YAML file (see `library.rs`).

mod library;
mod nodes;
mod state;
mod yaml;

pub use library::load;
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
    /// Test helper: the node `route` picks after `current`, given `status` as the
    /// last node's outcome. Lets the YAML loader's tests assert routing without
    /// running nodes.
    #[cfg(test)]
    pub(crate) fn route_with_status_for_test(
        &self,
        current: &str,
        status: Option<Status>,
    ) -> String {
        let mut state = State::new(String::new());
        state.status = status;
        Executor::route(self, current, &state).unwrap()
    }

    /// Test helper: routing after `current` with no prior status (treated as
    /// success by conditional routers).
    #[cfg(test)]
    pub(crate) fn route_for_test(&self, current: &str) -> String {
        self.route_with_status_for_test(current, None)
    }

    pub fn builder(name: impl Into<String>, entry: impl Into<String>) -> GraphBuilder {
        GraphBuilder {
            name: name.into(),
            entry: entry.into(),
            nodes: HashMap::new(),
            edges: HashMap::new(),
            duplicate_nodes: Vec::new(),
            duplicate_edges: Vec::new(),
        }
    }
}

/// Builder for a [`Graph`], validated on [`GraphBuilder::build`].
pub struct GraphBuilder {
    name: String,
    entry: String,
    nodes: HashMap<String, Box<dyn Node>>,
    edges: HashMap<String, Routing>,
    /// Node names added more than once; rejected by `build`.
    duplicate_nodes: Vec<String>,
    /// Edge sources given more than one outgoing edge; rejected by `build`.
    duplicate_edges: Vec<String>,
}

impl GraphBuilder {
    /// Add a node, keyed by its own name. Adding two nodes with the same name is
    /// a mistake that `build` rejects (rather than silently dropping one).
    pub fn node(mut self, node: impl Node + 'static) -> Self {
        let name = node.name().to_string();
        if self.nodes.insert(name.clone(), Box::new(node)).is_some() {
            self.duplicate_nodes.push(name);
        }
        self
    }

    /// Add an unconditional edge `from -> to` (`to` may be [`END`]). Defining
    /// more than one outgoing edge for the same `from` is rejected by `build`.
    pub fn edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.insert_edge(from.into(), Routing::Static(to.into()));
        self
    }

    /// Add a conditional edge: after `from`, `router(state)` picks the next node
    /// (or [`END`]). Defining more than one outgoing edge for the same `from` is
    /// rejected by `build`.
    pub fn conditional(
        mut self,
        from: impl Into<String>,
        router: impl Fn(&State) -> String + Send + Sync + 'static,
    ) -> Self {
        self.insert_edge(from.into(), Routing::Conditional(Box::new(router)));
        self
    }

    fn insert_edge(&mut self, from: String, routing: Routing) {
        if self.edges.insert(from.clone(), routing).is_some() {
            self.duplicate_edges.push(from);
        }
    }

    /// Validate and finalize the graph. Checks that the entry and all static
    /// edge endpoints refer to known nodes (or END). Conditional targets are
    /// checked at run time.
    pub fn build(mut self) -> Result<Graph, BlueprintError> {
        let known = |id: &str| id == END || self.nodes.contains_key(id);

        if let Some(dupes) = dedup_names(&mut self.duplicate_nodes) {
            return Err(BlueprintError::Invalid(format!(
                "duplicate node name(s): {dupes}"
            )));
        }
        if let Some(dupes) = dedup_names(&mut self.duplicate_edges) {
            return Err(BlueprintError::Invalid(format!(
                "node(s) with more than one outgoing edge: {dupes}"
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

/// Aborts a spawned task when dropped, so the Ctrl-C watcher is always cleaned
/// up even if the run returns early or panics.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Sort and de-duplicate collected duplicate names; return them joined for an
/// error message, or `None` if there were none.
fn dedup_names(names: &mut Vec<String>) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    names.sort();
    names.dedup();
    Some(names.join(", "))
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

    /// Cap on executed nodes before the run aborts, bounding cyclic graphs.
    /// Defaults to [`DEFAULT_MAX_STEPS`].
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Emit an audit event, building it lazily so no event (and its string
    /// allocations) is constructed when no sink is attached.
    fn emit(&self, event: impl FnOnce() -> AuditEvent) {
        if let Some(audit) = &self.audit {
            audit.record(event());
        }
    }

    /// Build the per-run node context that borrows this executor's shared
    /// resources and agent-node configuration. Centralized so adding a field to
    /// both `Executor` and `NodeCtx` has a single mapping site.
    fn node_ctx(&self, cancel: CancellationToken) -> NodeCtx<'_> {
        NodeCtx {
            provider: self.provider.clone(),
            tools: &self.tools,
            audit: self.audit.clone(),
            policy: &self.policy,
            system_prompt: self.system_prompt.as_deref(),
            max_iterations: self.max_iterations,
            compact_threshold: self.compact_threshold,
            cancel,
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
        // Abort the watcher on every exit path, including a panic unwind, so it
        // can't leak across runs.
        let _guard = AbortOnDrop(watcher);
        self.run_with_cancel(graph, task, cancel).await
    }

    /// Run `graph` under an external cancellation token.
    pub async fn run_with_cancel(
        &self,
        graph: &Graph,
        task: String,
        cancel: CancellationToken,
    ) -> anyhow::Result<State> {
        // A zero step budget can't run anything; report it as the configuration
        // error it is rather than a misleading "step limit exceeded".
        if self.max_steps == 0 {
            return Err(
                BlueprintError::Invalid("max_steps must be greater than 0".to_string()).into(),
            );
        }

        info!("Starting blueprint `{}` on task: {task}", graph.name);
        self.emit(|| AuditEvent::BlueprintStarted {
            blueprint: graph.name.clone(),
            entry: graph.entry.clone(),
        });

        let mut state = State::new(task);
        let cx = self.node_ctx(cancel.clone());

        // `steps` in the completion events is the number of nodes executed. The
        // normal path reaches END at the top of iteration `step` having run `step`
        // nodes; the cancelled path is mid-iteration having run `step + 1`; the
        // step-limit path has run `max_steps`. All three are that same count.
        let mut current = graph.entry.clone();
        for step in 0..self.max_steps {
            if current == END {
                self.emit(|| AuditEvent::BlueprintCompleted {
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
                    self.emit(|| AuditEvent::BlueprintCompleted {
                        steps: step + 1,
                        reason: CompletionReason::Error,
                    });
                    return Err(e.context(format!("blueprint node `{current}` failed")));
                }
            };
            // Record the status this node reported, before merging, so the audit
            // event reflects the node itself rather than depending on the `status`
            // channel's reducer staying last-write.
            let node_status = update.status;
            state.apply(update);
            let status = match node_status {
                Some(Status::Failed) => NodeStatus::Failed,
                _ => NodeStatus::Ok,
            };

            // Stop if cancelled while the node was running.
            if cancel.is_cancelled() {
                warn!("Blueprint cancelled at node `{current}`");
                self.emit(|| AuditEvent::BlueprintCompleted {
                    steps: step + 1,
                    reason: CompletionReason::Cancelled,
                });
                return Ok(state);
            }

            let next = match Self::route(graph, &current, &state) {
                Ok(next) => next,
                Err(e) => {
                    // The node did run; record it (with no successor) before the
                    // terminal event so the audit trail isn't missing this step.
                    self.emit(|| AuditEvent::NodeExecuted {
                        step: step + 1,
                        node: current.clone(),
                        kind,
                        status,
                        next: String::new(),
                    });
                    self.emit(|| AuditEvent::BlueprintCompleted {
                        steps: step + 1,
                        reason: CompletionReason::Error,
                    });
                    return Err(e.context(format!("routing after node `{current}`")));
                }
            };
            self.emit(|| AuditEvent::NodeExecuted {
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
        self.emit(|| AuditEvent::BlueprintCompleted {
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

        let err = executor()
            .with_max_steps(5)
            .run(&graph, "task".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("step limit"));
        assert_eq!(runs.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn zero_max_steps_is_a_config_error() {
        let runs = Arc::new(AtomicUsize::new(0));
        let graph = Graph::builder("g", "a")
            .node(CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs: runs.clone(),
            })
            .edge("a", END)
            .build()
            .unwrap();

        let err = executor()
            .with_max_steps(0)
            .run(&graph, "task".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max_steps must be greater than 0"));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "no node should run");
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

    #[test]
    fn build_rejects_duplicate_edges() {
        let runs = Arc::new(AtomicUsize::new(0));
        let result = Graph::builder("g", "a")
            .node(CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs,
            })
            .edge("a", END)
            .edge("a", END) // second outgoing edge for `a`
            .build();
        assert!(
            matches!(result, Err(BlueprintError::Invalid(msg)) if msg.contains("more than one outgoing edge"))
        );
    }
}
