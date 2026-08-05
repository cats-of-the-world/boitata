//! Shared server state and the in-memory run registry.
//!
//! v1 keeps runs in memory: a `HashMap` of run id to [`RunHandle`]. Each handle
//! carries the live event plumbing (broadcast + history), a cancel token, a
//! `finished` token the SSE stream waits on, and — once the run ends — a
//! [`RunResult`]. Restarting the server forgets past runs; persistence is a later
//! step.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use boitata_agent::TaskResult;
use boitata_core::config::Config;
use boitata_core::provider::Provider;
use boitata_core::tools::{ToolPolicy, ToolRegistry};
use boitata_orchestrator::{State as BlueprintState, Status as BlueprintStatus};
use boitata_store::Store;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::{History, RunEvent};

/// Everything an HTTP handler needs. Cheap to clone: providers/policy are behind
/// `Arc`, the tool registry is `Arc`-backed, and the registry is a shared lock.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub provider: Arc<dyn Provider>,
    /// Base tool set; cloned per run so concurrent runs don't share mutable state.
    pub tools: ToolRegistry,
    pub policy: Arc<ToolPolicy>,
    pub runs: Arc<RwLock<HashMap<Uuid, Arc<RunHandle>>>>,
    /// Blueprints the server can run, by name → file path. Populated from the
    /// `--blueprints-dir` directory at startup (empty when none is configured).
    /// Only these vetted names are accepted over the network — never an arbitrary
    /// path — so a run request can't read a file outside this set.
    pub blueprints: Arc<BTreeMap<String, PathBuf>>,
    /// Durable state database. Blueprint runs are checkpointed here so an
    /// interrupted run can be resumed (`POST /api/runs/{id}/resume`), including
    /// after a server restart when it's no longer in the in-memory registry.
    pub store: Store,
}

impl AppState {
    pub fn new(
        config: Config,
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        policy: ToolPolicy,
        blueprints: BTreeMap<String, PathBuf>,
        store: Store,
    ) -> Self {
        Self {
            config: Arc::new(config),
            provider,
            tools,
            policy: Arc::new(policy),
            runs: Arc::new(RwLock::new(HashMap::new())),
            blueprints: Arc::new(blueprints),
            store,
        }
    }

    pub fn get_run(&self, id: Uuid) -> Option<Arc<RunHandle>> {
        self.runs.read().unwrap().get(&id).cloned()
    }

    /// Register a run, evicting the oldest *finished* runs once the registry
    /// exceeds [`MAX_RUNS`] so a long-lived server's memory stays bounded.
    /// In-flight runs are never evicted. (v1 is in-memory only; the target model
    /// moves execution into ephemeral containers and replaces this registry.)
    pub fn register_run(&self, handle: Arc<RunHandle>) {
        let mut runs = self.runs.write().unwrap();
        runs.insert(handle.id, handle);
        if runs.len() > MAX_RUNS {
            let mut finished: Vec<(Uuid, DateTime<Utc>)> = runs
                .values()
                .filter(|h| !matches!(*h.status.read().unwrap(), RunStatus::Running))
                .map(|h| (h.id, h.started_at))
                .collect();
            finished.sort_by_key(|&(_, started)| started); // oldest first
            for (id, _) in finished.into_iter().take(runs.len() - MAX_RUNS) {
                runs.remove(&id);
            }
        }
    }
}

/// Upper bound on retained runs before finished ones are evicted oldest-first.
const MAX_RUNS: usize = 1000;

/// Lifecycle state of a run, tagged for JSON (`{"state": "running"}`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed { error: Option<String> },
    Cancelled,
    /// Interrupted (cancelled or crashed) but has a persisted checkpoint, so it
    /// can be resumed via `POST /api/runs/{id}/resume`. Used for runs surfaced
    /// from the state database that are no longer in the in-memory registry.
    Suspended,
}

/// A single run's shared handle. Fields behind locks are written by the run's
/// background task and read by HTTP handlers.
pub struct RunHandle {
    pub id: Uuid,
    pub task: String,
    pub blueprint: Option<String>,
    pub started_at: DateTime<Utc>,
    pub status: RwLock<RunStatus>,
    pub result: RwLock<Option<RunResult>>,
    /// Cancels the underlying agent/executor run.
    pub cancel: CancellationToken,
    /// Fired when the background task has fully finished and written its result;
    /// the SSE stream ends when this trips.
    pub finished: CancellationToken,
    pub tx: broadcast::Sender<RunEvent>,
    pub history: History,
}

impl RunHandle {
    pub fn summary(&self) -> RunSummary {
        RunSummary {
            id: self.id,
            task: self.task.clone(),
            blueprint: self.blueprint.clone(),
            status: self.status.read().unwrap().clone(),
            started_at: self.started_at,
        }
    }
}

/// Compact listing entry for `GET /api/runs`.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: Uuid,
    pub task: String,
    pub blueprint: Option<String>,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
}

/// One transcript entry from a blueprint run.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptEntry {
    pub node: String,
    pub text: String,
}

/// A finished tool call from an agent run.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: String,
    pub result: String,
    pub is_error: bool,
}

/// The final outcome of a run, unified across the agent and blueprint paths.
/// Fields specific to one path stay empty/`None` for the other.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RunResult {
    pub success: bool,
    pub final_message: Option<String>,
    pub error: Option<String>,
    /// Agent path only.
    pub iterations: Option<usize>,
    /// Agent path only.
    pub tool_calls: Vec<ToolCall>,
    /// Blueprint path only.
    pub transcript: Vec<TranscriptEntry>,
}

impl RunResult {
    /// Map an agent [`TaskResult`] to the wire shape.
    pub fn from_task(res: TaskResult) -> Self {
        Self {
            success: res.success,
            final_message: res.final_message,
            error: res.error,
            iterations: Some(res.iterations),
            tool_calls: res
                .tool_calls
                .into_iter()
                .map(|c| ToolCall {
                    name: c.name,
                    arguments: c.arguments,
                    result: c.result,
                    is_error: c.is_error,
                })
                .collect(),
            transcript: Vec::new(),
        }
    }

    /// Map a blueprint [`BlueprintState`] to the wire shape. The final message is
    /// the last transcript entry; success follows the terminal status.
    pub fn from_state(state: &BlueprintState) -> Self {
        let transcript: Vec<TranscriptEntry> = state
            .transcript()
            .map(|(node, text)| TranscriptEntry {
                node: node.to_string(),
                text: text.to_string(),
            })
            .collect();
        let final_message = transcript.last().map(|e| e.text.clone());
        let success = matches!(state.status, Some(BlueprintStatus::Ok));
        let error = match state.status {
            Some(BlueprintStatus::Failed) => Some("blueprint finished with a failing step".into()),
            None => Some("blueprint finished with no node having run".into()),
            Some(BlueprintStatus::Ok) => None,
        };
        Self {
            success,
            final_message,
            error,
            iterations: None,
            tool_calls: Vec::new(),
            transcript,
        }
    }
}
