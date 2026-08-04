// Blueprint: a hybrid deterministic/agentic workflow engine. A blueprint is a
// graph of nodes run over a typed state, from an entry node until END, with
// static and conditional edges and cycles. The design follows LangGraph, adapted
// so a node can be an agent, a tool, a script, or a human-input prompt.
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
//   - human:  pauses for operator input (human-in-the-loop)
//
// Edges: `Static(from -> [to, ...])` or `Conditional(from -> router(state) ->
// [next, ...])`. A source may fan out to several successors; a target may be the
// `END` sentinel; a node with no outgoing edge ends its path.
//
// The executor advances in super-steps (a Pregel/LangGraph-style model): each
// super-step runs the whole frontier (the active node set) concurrently, merges
// their updates through the channel reducers, then routes each node — the union
// of successors (minus END, de-duplicated) becomes the next frontier. This gives
// fan-out (a node with several successors) and fan-in (several predecessors of
// one node collapse to a single run). The run ends when the frontier empties, or
// at a step limit (`max_steps`, which bounds cyclic graphs). Ctrl-C cancels the
// run, and each super-step emits audit events (see `boitata_core::audit`).
//
// Blueprints are defined in YAML (see `yaml.rs`). A small starter library ships
// embedded in the binary, and `--blueprint` also accepts a path to a user's own
// YAML file (see `library.rs`).

mod container;
mod human;
mod library;
mod nodes;
mod sandbox;
mod state;
mod yaml;

pub use human::{HumanInterface, StdioHuman};
pub use library::{discover, load};
pub use sandbox::Sandbox;
pub use state::{State, Status};

use nodes::{Node, NodeCtx};
use sandbox::Sandboxes;
use state::Update;
use std::collections::HashMap;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::join_all;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use boitata_core::audit::{AuditEvent, AuditSink, CompletionReason, NodeKind, NodeStatus};
use boitata_core::provider::Provider;
use boitata_core::tools::{ToolPolicy, ToolRegistry};

/// Sentinel target that ends a blueprint run.
pub const END: &str = "__end__";

/// Default cap on executed nodes, so a cyclic graph can't loop forever.
const DEFAULT_MAX_STEPS: usize = 50;

/// How the run proceeds after a node finishes: the set of successor nodes to
/// activate. Returning several is a fan-out; an [`END`] entry (or an empty set)
/// terminates that path. The executor drops [`END`], de-duplicates, and unions
/// these across the nodes that ran to form the next super-step's frontier.
type Router = Box<dyn Fn(&State) -> Vec<String> + Send + Sync>;

enum Routing {
    /// Activate a fixed set of successors (one is the common case; more than one
    /// is a fan-out). Any entry may be [`END`].
    Static(Vec<String>),
    /// Compute the successor set from the state.
    Conditional(Router),
}

/// One node's outcome in a super-step: identity plus audit/routing data. Built
/// by [`Executor::run_frontier`] as it merges each node's `Update` into the
/// state, then consumed by `run_graph` to emit the `NodeExecuted` audit event
/// and route to the next frontier. `output` is the node's emitted text
/// (container stdout/stderr, script output, an agent's final message, …) — kept
/// here so a failing node's reason reaches the audit trail instead of being
/// dropped after the state merge.
struct RanNode {
    name: String,
    kind: NodeKind,
    status: NodeStatus,
    output: String,
}

/// A hard error from a super-step, tagged with the node that produced it and its
/// kind. Attached as [`anyhow`] context by [`Executor::run_frontier`] so
/// `run_graph` can recover the node's identity (via `downcast_ref`) and surface
/// the failure as a `NodeExecuted` audit event — otherwise a hard error leaves no
/// trace in the stream between "started" and "finished". Its `Display` is the same
/// message the plain context string used, so error output is unchanged.
#[derive(Debug)]
struct NodeFailure {
    node: String,
    kind: NodeKind,
}

impl std::fmt::Display for NodeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blueprint node `{}` failed", self.node)
    }
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
    /// Test helper: the successor set `route` picks after `current`, given
    /// `status` as the last node's outcome. Lets the YAML loader's tests assert
    /// routing without running nodes.
    #[cfg(test)]
    pub(crate) fn route_with_status_for_test(
        &self,
        current: &str,
        status: Option<Status>,
    ) -> Vec<String> {
        let mut state = State::new(String::new());
        state.status = status;
        Executor::route(self, current, &state).unwrap()
    }

    /// Test helper: routing after `current` with no prior status (treated as
    /// success by conditional routers).
    #[cfg(test)]
    pub(crate) fn route_for_test(&self, current: &str) -> Vec<String> {
        self.route_with_status_for_test(current, None)
    }

    pub fn builder(name: impl Into<String>, entry: impl Into<String>) -> GraphBuilder {
        GraphBuilder {
            name: name.into(),
            entry: entry.into(),
            nodes: HashMap::new(),
            edges: HashMap::new(),
            duplicate_nodes: Vec::new(),
            edge_conflicts: Vec::new(),
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
    /// Edge sources whose routing is contradictory — a conditional combined with
    /// any other edge (fan-out static edges from one source are fine, but they
    /// can't be mixed with a router). Rejected by `build`.
    edge_conflicts: Vec<String>,
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

    /// Add an unconditional edge `from -> to` (`to` may be [`END`]). Calling this
    /// more than once for the same `from` fans out to all the targets; mixing it
    /// with a conditional edge on the same `from` is rejected by `build`.
    pub fn edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        let from = from.into();
        match self.edges.get_mut(&from) {
            Some(Routing::Static(targets)) => targets.push(to.into()),
            // A router already governs this source; a static edge can't be added.
            Some(Routing::Conditional(_)) => self.edge_conflicts.push(from),
            None => {
                self.edges.insert(from, Routing::Static(vec![to.into()]));
            }
        }
        self
    }

    /// Add a conditional edge: after `from`, `router(state)` picks the successor
    /// set (each may be [`END`]; several is a fan-out). A source may have at most
    /// one router and no other edges; violations are rejected by `build`.
    pub fn conditional(
        mut self,
        from: impl Into<String>,
        router: impl Fn(&State) -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        let from = from.into();
        // A conditional must be the sole routing for its source: if anything is
        // already registered (static or another router), that's a conflict.
        if self
            .edges
            .insert(from.clone(), Routing::Conditional(Box::new(router)))
            .is_some()
        {
            self.edge_conflicts.push(from);
        }
        self
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
        if let Some(dupes) = dedup_names(&mut self.edge_conflicts) {
            return Err(BlueprintError::Invalid(format!(
                "node(s) with a conditional edge mixed with other edges: {dupes}"
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
            if let Routing::Static(targets) = routing {
                for to in targets {
                    if !known(to) {
                        return Err(BlueprintError::Invalid(format!(
                            "edge `{from}` -> `{to}` targets an unknown node"
                        )));
                    }
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

/// Exponential backoff before the `attempt`-th super-step retry (1-based):
/// 250ms, 500ms, 1s, 2s, 4s, capped at 5s.
fn retry_backoff(attempt: usize) -> Duration {
    const BASE: Duration = Duration::from_millis(250);
    const CAP: Duration = Duration::from_secs(5);
    // Bound the shift so `1 << shift` can't overflow; `saturating_mul` and the
    // cap keep the result sane for any larger attempt count.
    let shift = (attempt.saturating_sub(1)).min(16) as u32;
    BASE.saturating_mul(1u32 << shift).min(CAP)
}

/// Render a node's successor set for the audit `next` field: node names joined
/// by `+`, with the [`END`] sentinel shown as `END`. An empty set (a path that
/// ends) renders as `END` too.
fn display_routes(routes: &[String]) -> String {
    if routes.is_empty() {
        return "END".to_string();
    }
    routes
        .iter()
        .map(|t| if t == END { "END" } else { t.as_str() })
        .collect::<Vec<_>>()
        .join("+")
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
    /// How many times to retry a super-step that fails with a hard error before
    /// giving up. `0` (the default) means no retries.
    max_retries: usize,
    /// How `human` nodes collect operator input. Defaults to [`StdioHuman`].
    human: Arc<dyn HumanInterface>,
    /// Sandbox backend for the provisioning nodes; `None` means the default
    /// (local Docker). Overridden for other backends (e.g. Firecracker) or tests.
    sandbox_backend: Option<Arc<dyn Sandbox>>,
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
            max_retries: 0,
            human: Arc::new(StdioHuman::new()),
            sandbox_backend: None,
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

    /// Cap on super-steps before the run aborts, bounding cyclic graphs.
    /// Defaults to [`DEFAULT_MAX_STEPS`].
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// How many times to retry a super-step that fails with a hard node error,
    /// each attempt restoring the pre-step state. `None` (or `0`) means no
    /// retries.
    pub fn with_max_retries(mut self, max_retries: Option<usize>) -> Self {
        self.max_retries = max_retries.unwrap_or(0);
        self
    }

    /// Override how `human` nodes collect input (the default is [`StdioHuman`]).
    /// Used to inject a scripted responder in tests.
    pub fn with_human(mut self, human: Arc<dyn HumanInterface>) -> Self {
        self.human = human;
        self
    }

    /// Use a specific sandbox backend for provisioning nodes (default: Docker).
    pub fn with_sandbox(mut self, backend: Arc<dyn Sandbox>) -> Self {
        self.sandbox_backend = Some(backend);
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
    fn node_ctx(&self, cancel: CancellationToken, sandbox: Arc<Sandboxes>) -> NodeCtx<'_> {
        NodeCtx {
            provider: self.provider.clone(),
            tools: &self.tools,
            audit: self.audit.clone(),
            policy: &self.policy,
            system_prompt: self.system_prompt.as_deref(),
            max_iterations: self.max_iterations,
            compact_threshold: self.compact_threshold,
            human: self.human.clone(),
            sandbox,
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

    /// Run `graph` under an external cancellation token. Every container the run
    /// provisions is destroyed when it ends, for any reason — success, a failing
    /// step, a hard error, cancellation, or even a panic in a node's `run`.
    pub async fn run_with_cancel(
        &self,
        graph: &Graph,
        task: String,
        cancel: CancellationToken,
    ) -> anyhow::Result<State> {
        let sandbox = Arc::new(match &self.sandbox_backend {
            Some(backend) => Sandboxes::new(backend.clone()),
            None => Sandboxes::with_docker(),
        });
        // Catch a panic in the graph loop so cleanup still runs, then re-raise it.
        // Sandbox ids are recorded on `sandbox` as they're provisioned, so
        // `cleanup_all` covers them regardless of where the panic happened.
        let result = AssertUnwindSafe(self.run_graph(graph, task, cancel, &sandbox))
            .catch_unwind()
            .await;
        sandbox.cleanup_all().await;
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// The graph-execution core. `run_with_cancel` wraps this to guarantee
    /// container cleanup regardless of how it returns.
    async fn run_graph(
        &self,
        graph: &Graph,
        task: String,
        cancel: CancellationToken,
        sandbox: &Arc<Sandboxes>,
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
        let cx = self.node_ctx(cancel.clone(), sandbox.clone());

        // The run advances in super-steps: each super-step runs the whole
        // frontier (the active node set) concurrently, merges their updates, then
        // routes each to its successors — the union of which (minus END, dedup'd)
        // is the next frontier. `steps` in the completion events counts
        // super-steps; for a linear graph that equals nodes executed.
        let mut frontier = vec![graph.entry.clone()];
        for step in 0..self.max_steps {
            // A stable order makes concurrent merges and audit events
            // deterministic, and dedups a node reached by several predecessors
            // (fan-in) so it runs once per super-step.
            frontier.sort();
            frontier.dedup();

            if frontier.is_empty() {
                self.emit(|| AuditEvent::BlueprintCompleted {
                    steps: step,
                    reason: CompletionReason::Completed,
                });
                return Ok(state);
            }
            debug!("Blueprint super-step {}: frontier {frontier:?}", step + 1);

            // Run the frontier, checkpointing the pre-step state so a hard node
            // error can be retried from a clean slate up to `max_retries` times.
            // (A soft `Failed` status is not an error — the graph routes on it;
            // only a node returning `Err` — e.g. a transient provider failure —
            // triggers a retry.) Each attempt re-runs the whole super-step.
            let checkpoint = state.clone();
            let ran = {
                let mut attempt = 0;
                loop {
                    let mut trial = checkpoint.clone();
                    match Self::run_frontier(graph, &cx, &frontier, &mut trial).await {
                        Ok(ran) => {
                            state = trial;
                            break ran;
                        }
                        // A cancelled run reports as cancelled, not as a retryable
                        // failure — the error here is just the interrupted node.
                        Err(_) if cancel.is_cancelled() => {
                            warn!("Blueprint cancelled during super-step {}", step + 1);
                            self.emit(|| AuditEvent::BlueprintCompleted {
                                steps: step + 1,
                                reason: CompletionReason::Cancelled,
                            });
                            return Ok(state);
                        }
                        Err(e) if attempt < self.max_retries => {
                            attempt += 1;
                            warn!(
                                "Blueprint super-step {} failed (retry {attempt}/{}): {e:#}",
                                step + 1,
                                self.max_retries
                            );
                            self.emit(|| AuditEvent::SuperStepRetried {
                                step: step + 1,
                                attempt,
                                error: format!("{e:#}"),
                            });
                            // Back off before retrying — an instant retry against
                            // a rate-limited/overloaded provider just fails again —
                            // while staying responsive to cancellation.
                            tokio::select! {
                                _ = cancel.cancelled() => {
                                    self.emit(|| AuditEvent::BlueprintCompleted {
                                        steps: step + 1,
                                        reason: CompletionReason::Cancelled,
                                    });
                                    return Ok(state);
                                }
                                _ = tokio::time::sleep(retry_backoff(attempt)) => {}
                            }
                        }
                        Err(e) => {
                            // Surface the failing node's error as a node event so
                            // the reason is visible in the stream — a hard error
                            // otherwise leaves nothing between "started" and
                            // "finished". The node's identity rides on the error as
                            // `NodeFailure` context (see `run_frontier`).
                            if let Some(f) = e.downcast_ref::<NodeFailure>() {
                                let (node, kind) = (f.node.clone(), f.kind);
                                let output = format!("{e:#}");
                                self.emit(move || AuditEvent::NodeExecuted {
                                    step: step + 1,
                                    node,
                                    kind,
                                    status: NodeStatus::Failed,
                                    next: display_routes(&[]),
                                    output,
                                });
                            }
                            self.emit(|| AuditEvent::BlueprintCompleted {
                                steps: step + 1,
                                reason: CompletionReason::Error,
                            });
                            return Err(e);
                        }
                    }
                }
            };

            // Stop if cancelled while the super-step was running.
            if cancel.is_cancelled() {
                warn!("Blueprint cancelled during super-step {}", step + 1);
                self.emit(|| AuditEvent::BlueprintCompleted {
                    steps: step + 1,
                    reason: CompletionReason::Cancelled,
                });
                return Ok(state);
            }

            // Route every node that ran; the live (non-END) successors form the
            // next frontier.
            let mut next = Vec::new();
            for RanNode {
                name,
                kind,
                status,
                output,
            } in ran
            {
                let routes = match Self::route(graph, &name, &state) {
                    Ok(routes) => routes,
                    Err(e) => {
                        self.emit(|| AuditEvent::NodeExecuted {
                            step: step + 1,
                            node: name.clone(),
                            kind,
                            status,
                            next: String::new(),
                            output: output.clone(),
                        });
                        self.emit(|| AuditEvent::BlueprintCompleted {
                            steps: step + 1,
                            reason: CompletionReason::Error,
                        });
                        return Err(e.context(format!("routing after node `{name}`")));
                    }
                };
                self.emit(|| AuditEvent::NodeExecuted {
                    step: step + 1,
                    node: name.clone(),
                    kind,
                    status,
                    next: display_routes(&routes),
                    output: output.clone(),
                });
                next.extend(routes.into_iter().filter(|t| t != END));
            }
            frontier = next;
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

    /// The successor set after `current`: the node's static targets, its router's
    /// output, or `[END]` when it has no outgoing edge. Every non-END target is
    /// checked to be a real node.
    fn route(graph: &Graph, current: &str, state: &State) -> anyhow::Result<Vec<String>> {
        let targets = match graph.edges.get(current) {
            Some(Routing::Static(targets)) => targets.clone(),
            Some(Routing::Conditional(router)) => router(state),
            None => vec![END.to_string()],
        };
        for target in &targets {
            if target != END && !graph.nodes.contains_key(target) {
                return Err(BlueprintError::UnknownNode(target.clone()).into());
            }
        }
        Ok(targets)
    }

    /// Run one super-step: execute every node in `frontier` concurrently against
    /// the (checkpointed) `state`, then merge their updates into it in a
    /// deterministic order. Returns each node's `(name, kind, status)` for the
    /// audit trail. A hard error from any node aborts the super-step before any
    /// merge (so `state` is left untouched) and cancels the sibling futures; the
    /// caller retries from a fresh checkpoint clone.
    async fn run_frontier(
        graph: &Graph,
        cx: &NodeCtx<'_>,
        frontier: &[String],
        state: &mut State,
    ) -> anyhow::Result<Vec<RanNode>> {
        // Resolve every node up front so an unknown one is a clean error rather
        // than a panic inside a future.
        let mut runnables = Vec::with_capacity(frontier.len());
        for name in frontier {
            let node = graph
                .nodes
                .get(name)
                .ok_or_else(|| BlueprintError::UnknownNode(name.clone()))?;
            runnables.push((name.clone(), node));
        }

        // Run the frontier concurrently against a shared read-only view of the
        // state, under a per-super-step child token. On the first hard failure we
        // cancel that token so the sibling nodes stop promptly — killing their
        // subprocesses / aborting their requests via the usual cancellation path,
        // rather than orphaning them (merely dropping a future stops polling but
        // doesn't signal cancellation). The child also cancels if the run-wide
        // token does.
        let child = cx.cancel.child_token();
        let child_cx = cx.with_cancel(child.clone());
        let snapshot: &State = state;
        // The first error to occur wins (secondary cancellation errors from the
        // siblings we just cancelled are ignored), so the reported cause is the
        // real one regardless of completion order.
        let first_error: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
        let results = join_all(runnables.iter().map(|(name, node)| {
            let child = &child;
            let child_cx = &child_cx;
            let first_error = &first_error;
            async move {
                match node.run(snapshot, child_cx).await {
                    Ok(update) => Some((name.clone(), node.kind(), update)),
                    Err(e) => {
                        let mut slot = first_error.lock().expect("first_error mutex poisoned");
                        if slot.is_none() {
                            *slot = Some(e.context(NodeFailure {
                                node: name.clone(),
                                kind: node.kind(),
                            }));
                            child.cancel();
                        }
                        None
                    }
                }
            }
        }))
        .await;

        if let Some(e) = first_error
            .into_inner()
            .expect("first_error mutex poisoned")
        {
            return Err(e);
        }
        // No error: every future produced an update, in sorted-frontier order.
        let outcomes: Vec<(String, NodeKind, Update)> = results.into_iter().flatten().collect();

        // Merge each node's update into the state. `vars` uses last-write-wins
        // (see `State::apply`); in a fan-out, results merge in sorted-frontier
        // order, so two parallel nodes writing the *same* `vars` key resolve
        // deterministically but by name order. In practice each node writes only
        // its own name key (unique), so this collides only for nodes that emit a
        // shared custom key.
        let mut ran: Vec<RanNode> = Vec::with_capacity(outcomes.len());
        let mut any_failed = false;
        let mut any_status = false;
        for (name, kind, update) in outcomes {
            let node_status = match update.status {
                Some(Status::Failed) => {
                    any_failed = true;
                    any_status = true;
                    NodeStatus::Failed
                }
                Some(Status::Ok) => {
                    any_status = true;
                    NodeStatus::Ok
                }
                None => NodeStatus::Ok,
            };
            // Capture the node's emitted text for the audit trail before the
            // update is moved into the state merge.
            let output = update
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            state.apply(update);
            ran.push(RanNode {
                name,
                kind,
                status: node_status,
                output,
            });
        }
        // Super-step status: failure dominates a fan-out, so verify-style routing
        // sees a failure if any parallel branch failed. For a single-node
        // super-step this is just that node's status.
        if any_status {
            state.status = Some(if any_failed {
                Status::Failed
            } else {
                Status::Ok
            });
        }
        Ok(ran)
    }
}

#[cfg(test)]
mod tests {
    use super::state::Update;
    use super::*;
    use async_trait::async_trait;
    use boitata_core::audit::NodeKind;
    use boitata_core::provider::{Chunk, CompletionRequest, CompletionResponse, ProviderResult};
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

    /// Best-effort sink that records every event, to assert what reaches the
    /// audit trail.
    #[derive(Default)]
    struct RecordingSink(parking_lot::Mutex<Vec<AuditEvent>>);

    impl AuditSink for RecordingSink {
        fn record(&self, event: AuditEvent) {
            self.0.lock().push(event);
        }
    }

    #[tokio::test]
    async fn failing_node_output_is_audited() {
        // Mirrors a containerized `clone` step: a node that fails and emits the
        // command output explaining why. That output must reach the
        // `NodeExecuted` event so a failure isn't a bare `(failed)`.
        struct FailingClone;
        #[async_trait]
        impl Node for FailingClone {
            fn name(&self) -> &str {
                "clone"
            }
            fn kind(&self) -> NodeKind {
                NodeKind::Container
            }
            async fn run(&self, _s: &State, _c: &NodeCtx<'_>) -> anyhow::Result<Update> {
                Ok(Update::from_node(
                    "clone",
                    "fatal: could not read remote repository\n".into(),
                    Status::Failed,
                ))
            }
        }

        let graph = Graph::builder("containerized_task", "clone")
            .node(FailingClone)
            .edge("clone", END)
            .build()
            .unwrap();

        let sink = Arc::new(RecordingSink::default());
        executor()
            .with_audit(sink.clone())
            .run(&graph, "t".into())
            .await
            .unwrap();

        let (status, output) = sink
            .0
            .lock()
            .iter()
            .find_map(|e| match e {
                AuditEvent::NodeExecuted {
                    node,
                    status,
                    output,
                    ..
                } if node == "clone" => Some((*status, output.clone())),
                _ => None,
            })
            .expect("a NodeExecuted event for the clone node");
        assert_eq!(status, NodeStatus::Failed);
        assert!(
            output.contains("could not read remote repository"),
            "NodeExecuted.output should carry the failure reason; got {output:?}"
        );
    }

    #[tokio::test]
    async fn hard_node_error_is_audited() {
        // A node that returns a hard `Err` (e.g. a provision step whose image
        // can't be pulled) must still surface a `NodeExecuted` (Failed) event
        // carrying the error, so the reason is visible in the stream rather than
        // only a bare `blueprint finished · error`.
        struct Boom;
        #[async_trait]
        impl Node for Boom {
            fn name(&self) -> &str {
                "provision"
            }
            fn kind(&self) -> NodeKind {
                NodeKind::Container
            }
            async fn run(&self, _s: &State, _c: &NodeCtx<'_>) -> anyhow::Result<Update> {
                anyhow::bail!("failed to pull image `nope:latest`: not found")
            }
        }

        let graph = Graph::builder("g", "provision")
            .node(Boom)
            .edge("provision", END)
            .build()
            .unwrap();

        let sink = Arc::new(RecordingSink::default());
        let err = executor()
            .with_audit(sink.clone())
            .run(&graph, "t".into())
            .await
            .unwrap_err();
        // The error still propagates, with the node named.
        assert!(format!("{err:#}").contains("provision"), "{err:#}");

        let (status, output) = sink
            .0
            .lock()
            .iter()
            .find_map(|e| match e {
                AuditEvent::NodeExecuted {
                    node,
                    status,
                    output,
                    ..
                } if node == "provision" => Some((*status, output.clone())),
                _ => None,
            })
            .expect("a NodeExecuted event for the failed provision node");
        assert_eq!(status, NodeStatus::Failed);
        assert!(
            output.contains("failed to pull image"),
            "hard-error output should carry the reason; got {output:?}"
        );
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
                Some(Status::Failed) => vec!["work".to_string()],
                _ => vec![END.to_string()],
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
            .conditional("check", |_state| vec![END.to_string()])
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
    fn build_rejects_conditional_mixed_with_other_edges() {
        // A conditional edge must be a source's sole routing: combining it with a
        // static edge (in either order) is a conflict. (Multiple *static* edges,
        // by contrast, are a valid fan-out.)
        let build = |static_first: bool| {
            let node = CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs: Arc::new(AtomicUsize::new(0)),
            };
            let b = Graph::builder("g", "a").node(node);
            let b = if static_first {
                b.edge("a", END).conditional("a", |_| vec![END.to_string()])
            } else {
                b.conditional("a", |_| vec![END.to_string()]).edge("a", END)
            };
            b.build()
        };
        for static_first in [true, false] {
            assert!(
                matches!(build(static_first), Err(BlueprintError::Invalid(msg)) if msg.contains("conditional edge mixed")),
                "static_first={static_first} should be a conflict"
            );
        }
    }

    #[tokio::test]
    async fn fan_out_and_fan_in_run_each_node_once() {
        // a -> {b, c} -> d (diamond). b and c run concurrently in one super-step;
        // d is reached from both but runs once (fan-in dedup).
        let counts: HashMap<&str, Arc<AtomicUsize>> = ["a", "b", "c", "d"]
            .iter()
            .map(|&n| (n, Arc::new(AtomicUsize::new(0))))
            .collect();
        let graph = Graph::builder("diamond", "a")
            .node(CountingNode {
                name: "a".into(),
                status: Status::Ok,
                runs: counts["a"].clone(),
            })
            .node(CountingNode {
                name: "b".into(),
                status: Status::Ok,
                runs: counts["b"].clone(),
            })
            .node(CountingNode {
                name: "c".into(),
                status: Status::Ok,
                runs: counts["c"].clone(),
            })
            .node(CountingNode {
                name: "d".into(),
                status: Status::Ok,
                runs: counts["d"].clone(),
            })
            .edge("a", "b")
            .edge("a", "c")
            .edge("b", "d")
            .edge("c", "d")
            .edge("d", END)
            .build()
            .unwrap();

        let state = executor().run(&graph, "task".into()).await.unwrap();
        for n in ["a", "b", "c", "d"] {
            assert_eq!(counts[n].load(Ordering::SeqCst), 1, "node {n} ran once");
        }
        // Every node contributed exactly one transcript entry.
        assert_eq!(state.messages.len(), 4);
    }

    // A node whose `run` returns a hard error its first `fail_times` runs, then
    // succeeds. Used to exercise super-step retry.
    struct FlakyErrNode {
        fail_times: usize,
        runs: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Node for FlakyErrNode {
        fn name(&self) -> &str {
            "flaky"
        }
        fn kind(&self) -> NodeKind {
            NodeKind::Script
        }
        async fn run(&self, _s: &State, _c: &NodeCtx<'_>) -> anyhow::Result<Update> {
            let n = self.runs.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                anyhow::bail!("transient failure {n}");
            }
            Ok(Update::from_node("flaky", "recovered".into(), Status::Ok))
        }
    }

    #[tokio::test]
    async fn retry_recovers_after_transient_errors() {
        // Fails twice, then succeeds; two retries are enough. The failed
        // attempts' partial state is discarded, so only one message survives.
        let runs = Arc::new(AtomicUsize::new(0));
        let graph = Graph::builder("retry", "flaky")
            .node(FlakyErrNode {
                fail_times: 2,
                runs: runs.clone(),
            })
            .edge("flaky", END)
            .build()
            .unwrap();

        let state = executor()
            .with_max_retries(Some(2))
            .run(&graph, "t".into())
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3, "2 failures + 1 success");
        assert_eq!(state.messages.len(), 1, "failed attempts leave no residue");
        assert_eq!(state.status, Some(Status::Ok));
    }

    #[tokio::test]
    async fn retry_exhausted_fails_the_run() {
        // Fails three times but only two retries are allowed → the run fails.
        let runs = Arc::new(AtomicUsize::new(0));
        let graph = Graph::builder("retry", "flaky")
            .node(FlakyErrNode {
                fail_times: 5,
                runs: runs.clone(),
            })
            .edge("flaky", END)
            .build()
            .unwrap();

        let err = executor()
            .with_max_retries(Some(2))
            .run(&graph, "t".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("flaky"), "{err}");
        assert_eq!(runs.load(Ordering::SeqCst), 3, "initial try + 2 retries");
    }

    // A [`HumanInterface`] that replays queued replies, erroring when exhausted
    // (standing in for a non-interactive stdin at EOF).
    struct ScriptedHuman {
        replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    }
    impl ScriptedHuman {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: std::sync::Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
            })
        }
    }
    #[async_trait]
    impl HumanInterface for ScriptedHuman {
        async fn prompt(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> anyhow::Result<String> {
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no scripted reply (stdin EOF)"))
        }
    }

    /// approve (human) -> work on approval, END on decline.
    fn approval_graph() -> Graph {
        use super::nodes::{HumanMode, HumanNode, ScriptNode};
        Graph::builder("h", "approve")
            .node(HumanNode::new(
                "approve",
                "proceed? {task}",
                HumanMode::Approval,
            ))
            .node(ScriptNode::new("work", "echo working"))
            .conditional("approve", |s| match s.status {
                Some(Status::Failed) => vec![END.to_string()],
                _ => vec!["work".to_string()],
            })
            .edge("work", END)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn human_approval_yes_proceeds() {
        let state = executor()
            .with_human(ScriptedHuman::new(&["yes"]))
            .run(&approval_graph(), "do it".into())
            .await
            .unwrap();
        assert_eq!(
            state.vars.get("approve").map(String::as_str),
            Some("approved")
        );
        assert!(state.vars.contains_key("work"), "work should have run");
    }

    #[tokio::test]
    async fn human_approval_no_aborts() {
        let state = executor()
            .with_human(ScriptedHuman::new(&["n"]))
            .run(&approval_graph(), "do it".into())
            .await
            .unwrap();
        assert_eq!(
            state.vars.get("approve").map(String::as_str),
            Some("declined")
        );
        assert!(!state.vars.contains_key("work"), "work must be skipped");
        assert_eq!(state.status, Some(Status::Failed));
    }

    #[tokio::test]
    async fn human_input_captures_reply() {
        use super::nodes::{HumanMode, HumanNode};
        let graph = Graph::builder("h", "ask")
            .node(HumanNode::new("ask", "your name?", HumanMode::Input))
            .edge("ask", END)
            .build()
            .unwrap();
        let state = executor()
            .with_human(ScriptedHuman::new(&["Ada"]))
            .run(&graph, "t".into())
            .await
            .unwrap();
        assert_eq!(state.vars.get("ask").map(String::as_str), Some("Ada"));
        assert_eq!(state.status, Some(Status::Ok));
    }

    #[tokio::test]
    async fn human_input_unavailable_is_a_hard_error() {
        // No scripted reply → the interface errors (as a non-interactive stdin
        // would), which is a hard error that fails the run rather than a decline.
        let err = executor()
            .with_human(ScriptedHuman::new(&[]))
            .run(&approval_graph(), "t".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("approve"), "{err}");
    }
}
