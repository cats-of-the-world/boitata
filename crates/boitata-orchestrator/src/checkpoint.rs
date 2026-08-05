//! Durable checkpointing: persist a blueprint run's state at each super-step
//! boundary so a cancelled or crashed run can be resumed instead of restarted.
//!
//! The executor writes a snapshot at the *top* of every super-step — the frontier
//! about to run plus the merged state so far — and flips the stored status when
//! the run ends. Resuming loads the latest snapshot and continues the super-step
//! loop from there. This mirrors the in-memory retry checkpoint the executor
//! already takes ([`State::apply`]-level isolation), so re-running the resumed
//! super-step is as safe as retrying one.
//!
//! [`Checkpointer`] is the abstraction; [`SqliteCheckpointer`] is the
//! `boitata-store`-backed implementation. Failing to persist a checkpoint never
//! aborts a run — the executor logs and continues, same as audit.
//!
//! Limitation: a run that provisioned an ephemeral sandbox can't be fully resumed
//! after the original process exits, because the container was torn down with it —
//! the saved state still references a now-dead container id. Resume is fully
//! correct for agent/tool/script/human graphs; sandbox resumption needs
//! persistent sandboxes (tracked separately).

use async_trait::async_trait;

use crate::state::State;
use boitata_store::{CheckpointUpsert, RunState, Store};

/// A resumable snapshot of a blueprint run at a super-step boundary.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// The run this snapshot belongs to (the executor's `run_id`).
    pub run_id: String,
    /// The graph's name, recorded so a resume can verify it's re-running the same
    /// blueprint.
    pub blueprint: String,
    /// The super-step index to resume at (the next step to run).
    pub step: usize,
    /// The active node set to run at `step`.
    pub frontier: Vec<String>,
    /// The merged graph state as of `step`.
    pub state: State,
    /// Whether this checkpoint represents a resumable run. A running snapshot the
    /// executor writes is always resumable (`true`); on load it reflects the
    /// stored status, so a resume can refuse a run that already completed/failed.
    pub resumable: bool,
}

/// Terminal (or paused) status a run's checkpoint is flipped to when it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointStatus {
    /// Interrupted deliberately (cancelled); still resumable.
    Suspended,
    /// Reached END successfully.
    Completed,
    /// Ended on a hard error or the step limit.
    Failed,
}

impl From<CheckpointStatus> for RunState {
    fn from(s: CheckpointStatus) -> Self {
        match s {
            CheckpointStatus::Suspended => RunState::Suspended,
            CheckpointStatus::Completed => RunState::Completed,
            CheckpointStatus::Failed => RunState::Failed,
        }
    }
}

/// Persists and restores blueprint run checkpoints.
#[async_trait]
pub trait Checkpointer: Send + Sync {
    /// Persist a pre-super-step snapshot, marking the run as still running.
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<()>;

    /// Load a run's latest checkpoint, if one exists.
    async fn load(&self, run_id: &str) -> anyhow::Result<Option<Checkpoint>>;

    /// Flip a run's stored status when it ends (or is suspended).
    async fn set_status(&self, run_id: &str, status: CheckpointStatus) -> anyhow::Result<()>;
}

/// A [`Checkpointer`] backed by the SQLite [`Store`]. Serializes [`State`] to JSON
/// for storage.
pub struct SqliteCheckpointer {
    store: Store,
}

impl SqliteCheckpointer {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Checkpointer for SqliteCheckpointer {
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        let state = serde_json::to_string(&checkpoint.state)?;
        self.store
            .upsert_checkpoint(CheckpointUpsert {
                run_id: checkpoint.run_id.clone(),
                blueprint: checkpoint.blueprint.clone(),
                task: checkpoint.state.task.clone(),
                step: checkpoint.step as u64,
                frontier: checkpoint.frontier.clone(),
                state,
                status: RunState::Running,
            })
            .await
    }

    async fn load(&self, run_id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let Some(record) = self.store.get_checkpoint(run_id).await? else {
            return Ok(None);
        };
        let state: State = serde_json::from_str(&record.state)?;
        Ok(Some(Checkpoint {
            run_id: record.run_id,
            blueprint: record.blueprint,
            step: record.step as usize,
            frontier: record.frontier,
            state,
            resumable: record.status.is_resumable(),
        }))
    }

    async fn set_status(&self, run_id: &str, status: CheckpointStatus) -> anyhow::Result<()> {
        self.store.set_checkpoint_status(run_id, status.into()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_load_roundtrips_state() {
        let store = Store::open_in_memory().unwrap();
        let cp = SqliteCheckpointer::new(store);

        let mut state = State::new("fix the bug".to_string());
        state.vars.insert("verify".to_string(), "exit 1".to_string());
        let checkpoint = Checkpoint {
            run_id: "run-1".to_string(),
            blueprint: "fix_test".to_string(),
            step: 2,
            frontier: vec!["fix".to_string()],
            state,
            resumable: true,
        };
        cp.save(&checkpoint).await.unwrap();

        let loaded = cp.load("run-1").await.unwrap().unwrap();
        assert!(loaded.resumable, "a saved running snapshot loads as resumable");
        assert_eq!(loaded.blueprint, "fix_test");
        assert_eq!(loaded.step, 2);
        assert_eq!(loaded.frontier, vec!["fix".to_string()]);
        assert_eq!(loaded.state.task, "fix the bug");
        assert_eq!(
            loaded.state.vars.get("verify").map(String::as_str),
            Some("exit 1")
        );
    }

    #[tokio::test]
    async fn load_missing_is_none() {
        let store = Store::open_in_memory().unwrap();
        let cp = SqliteCheckpointer::new(store);
        assert!(cp.load("nope").await.unwrap().is_none());
    }
}
