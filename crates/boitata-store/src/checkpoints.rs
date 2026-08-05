//! The `checkpoints` table: one row per blueprint run, holding enough to resume
//! it from the last completed super-step.
//!
//! `frontier` and `state` are opaque JSON the caller (the orchestrator) owns; the
//! store only reads/writes the text. `status` distinguishes a resumable run
//! (`Running` — the process may have crashed mid-run — or `Suspended`, e.g.
//! cancelled) from a terminal one (`Completed`/`Failed`).

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::Store;

/// Lifecycle of a checkpointed run, as stored in the `status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// The run is (or was) executing. If the process died, the row is left in
    /// this state and is still resumable.
    Running,
    /// The run was interrupted deliberately (e.g. cancelled) and can be resumed.
    Suspended,
    /// The run reached END successfully.
    Completed,
    /// The run ended on a hard error or the step limit.
    Failed,
}

impl RunState {
    fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Suspended => "suspended",
            RunState::Completed => "completed",
            RunState::Failed => "failed",
        }
    }

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "running" => RunState::Running,
            "suspended" => RunState::Suspended,
            "completed" => RunState::Completed,
            "failed" => RunState::Failed,
            other => bail!("unknown checkpoint status `{other}`"),
        })
    }

    /// Whether a run in this state can be resumed.
    pub fn is_resumable(self) -> bool {
        matches!(self, RunState::Running | RunState::Suspended)
    }
}

/// The fields needed to write (insert or replace) a checkpoint. `frontier` and
/// `state` are JSON the caller serialized; the store treats them as opaque text.
pub struct CheckpointUpsert {
    pub run_id: String,
    pub blueprint: String,
    pub task: String,
    /// The super-step index this checkpoint resumes at (the next step to run).
    pub step: u64,
    /// The active node set to run at `step`, as a JSON array of node names.
    pub frontier: Vec<String>,
    /// The serialized graph state as JSON.
    pub state: String,
    pub status: RunState,
}

/// A checkpoint row read back from the store.
#[derive(Debug, Clone)]
pub struct CheckpointRecord {
    pub run_id: String,
    pub blueprint: String,
    pub task: String,
    pub step: u64,
    pub frontier: Vec<String>,
    pub state: String,
    pub status: RunState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Store {
    /// Insert or replace the checkpoint for a run. `created_at` is preserved on
    /// replace; `updated_at` is set to now.
    pub async fn upsert_checkpoint(&self, cp: CheckpointUpsert) -> anyhow::Result<()> {
        let frontier = serde_json::to_string(&cp.frontier).context("serialize frontier")?;
        let now = Utc::now().to_rfc3339();
        self.call(move |conn| {
            conn.execute(
                "INSERT INTO checkpoints
                     (run_id, blueprint, task, step, frontier, state, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(run_id) DO UPDATE SET
                     blueprint  = excluded.blueprint,
                     task       = excluded.task,
                     step       = excluded.step,
                     frontier   = excluded.frontier,
                     state      = excluded.state,
                     status     = excluded.status,
                     updated_at = excluded.updated_at",
                params![
                    cp.run_id,
                    cp.blueprint,
                    cp.task,
                    cp.step,
                    frontier,
                    cp.state,
                    cp.status.as_str(),
                    now,
                ],
            )
            .context("insert checkpoint")?;
            Ok(())
        })
        .await
    }

    /// Update just the lifecycle status of a run's checkpoint (e.g. flip it to
    /// `Completed` when the run ends). No-op if the run has no checkpoint.
    pub async fn set_checkpoint_status(
        &self,
        run_id: &str,
        status: RunState,
    ) -> anyhow::Result<()> {
        let run_id = run_id.to_string();
        let now = Utc::now().to_rfc3339();
        self.call(move |conn| {
            conn.execute(
                "UPDATE checkpoints SET status = ?2, updated_at = ?3 WHERE run_id = ?1",
                params![run_id, status.as_str(), now],
            )
            .context("update checkpoint status")?;
            Ok(())
        })
        .await
    }

    /// Fetch a single run's checkpoint, if any.
    pub async fn get_checkpoint(&self, run_id: &str) -> anyhow::Result<Option<CheckpointRecord>> {
        let run_id = run_id.to_string();
        self.call(move |conn| {
            conn.query_row(
                "SELECT run_id, blueprint, task, step, frontier, state, status, created_at, updated_at
                 FROM checkpoints WHERE run_id = ?1",
                params![run_id],
                row_to_record,
            )
            .optional_record()
        })
        .await
    }

    /// List checkpoints, newest activity first. `resumable_only` filters to runs
    /// that can still be resumed (running/suspended).
    pub async fn list_checkpoints(
        &self,
        resumable_only: bool,
    ) -> anyhow::Result<Vec<CheckpointRecord>> {
        self.call(move |conn| {
            let sql = if resumable_only {
                "SELECT run_id, blueprint, task, step, frontier, state, status, created_at, updated_at
                 FROM checkpoints WHERE status IN ('running', 'suspended')
                 ORDER BY updated_at DESC"
            } else {
                "SELECT run_id, blueprint, task, step, frontier, state, status, created_at, updated_at
                 FROM checkpoints ORDER BY updated_at DESC"
            };
            let mut stmt = conn.prepare(sql).context("prepare list checkpoints")?;
            let rows = stmt
                .query_map([], row_to_record)
                .context("query checkpoints")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("read checkpoint row")?);
            }
            Ok(out)
        })
        .await
    }

    /// Remove a run's checkpoint entirely.
    pub async fn delete_checkpoint(&self, run_id: &str) -> anyhow::Result<()> {
        let run_id = run_id.to_string();
        self.call(move |conn| {
            conn.execute("DELETE FROM checkpoints WHERE run_id = ?1", params![run_id])
                .context("delete checkpoint")?;
            Ok(())
        })
        .await
    }
}

/// Map a `checkpoints` row (in the SELECT column order used above) to a record.
fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRecord> {
    let frontier: String = row.get(4)?;
    let status: String = row.get(6)?;
    let created_at: String = row.get(7)?;
    let updated_at: String = row.get(8)?;
    Ok(CheckpointRecord {
        run_id: row.get(0)?,
        blueprint: row.get(1)?,
        task: row.get(2)?,
        step: row.get(3)?,
        frontier: parse_frontier(&frontier, row)?,
        state: row.get(5)?,
        status: parse_status(&status, row)?,
        created_at: parse_time(&created_at, row)?,
        updated_at: parse_time(&updated_at, row)?,
    })
}

// The helpers below convert a stored TEXT column to its typed form, surfacing a
// corrupt value as a rusqlite `FromSqlConversionFailure` on the right column
// rather than a panic. `_row` is unused but kept in the signature so callers read
// as "parse column N of this row".
fn parse_frontier(s: &str, _row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(s).map_err(|e| conv_err(4, e))
}

fn parse_status(s: &str, _row: &rusqlite::Row<'_>) -> rusqlite::Result<RunState> {
    RunState::from_str(s).map_err(|e| conv_err(6, e))
}

fn parse_time(s: &str, _row: &rusqlite::Row<'_>) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| conv_err(7, e))
}

/// Wrap a conversion failure (from serde/chrono/our own parsing) as the rusqlite
/// error for column `idx`. The concrete error is stringified so this accepts
/// anything `Display`, including `anyhow::Error`.
fn conv_err(idx: usize, e: impl std::fmt::Display) -> rusqlite::Error {
    let msg = std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string());
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(msg))
}

/// Small extension to turn a "no rows" error from `query_row` into `Ok(None)`.
trait OptionalRecord {
    fn optional_record(self) -> anyhow::Result<Option<CheckpointRecord>>;
}

impl OptionalRecord for rusqlite::Result<CheckpointRecord> {
    fn optional_record(self) -> anyhow::Result<Option<CheckpointRecord>> {
        match self {
            Ok(rec) => Ok(Some(rec)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e).context("read checkpoint")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(run_id: &str, status: RunState) -> CheckpointUpsert {
        CheckpointUpsert {
            run_id: run_id.to_string(),
            blueprint: "fix_test".to_string(),
            task: "make it pass".to_string(),
            step: 3,
            frontier: vec!["test".to_string(), "fix".to_string()],
            state: r#"{"task":"make it pass"}"#.to_string(),
            status,
        }
    }

    #[tokio::test]
    async fn upsert_then_get_roundtrips() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_checkpoint(sample("run-1", RunState::Running)).await.unwrap();

        let got = store.get_checkpoint("run-1").await.unwrap().unwrap();
        assert_eq!(got.run_id, "run-1");
        assert_eq!(got.blueprint, "fix_test");
        assert_eq!(got.step, 3);
        assert_eq!(got.frontier, vec!["test".to_string(), "fix".to_string()]);
        assert_eq!(got.status, RunState::Running);
        assert!(got.status.is_resumable());
    }

    #[tokio::test]
    async fn get_missing_is_none() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_checkpoint("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_replaces_and_preserves_created_at() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_checkpoint(sample("run-1", RunState::Running)).await.unwrap();
        let first = store.get_checkpoint("run-1").await.unwrap().unwrap();

        let mut next = sample("run-1", RunState::Running);
        next.step = 7;
        store.upsert_checkpoint(next).await.unwrap();
        let second = store.get_checkpoint("run-1").await.unwrap().unwrap();

        assert_eq!(second.step, 7);
        assert_eq!(second.created_at, first.created_at, "created_at is preserved");
        assert!(second.updated_at >= first.updated_at);
    }

    #[tokio::test]
    async fn set_status_and_resumable_filter() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_checkpoint(sample("a", RunState::Running)).await.unwrap();
        store.upsert_checkpoint(sample("b", RunState::Running)).await.unwrap();
        store.set_checkpoint_status("b", RunState::Completed).await.unwrap();

        let resumable = store.list_checkpoints(true).await.unwrap();
        assert_eq!(resumable.len(), 1);
        assert_eq!(resumable[0].run_id, "a");

        let all = store.list_checkpoints(false).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn delete_removes_the_row() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_checkpoint(sample("a", RunState::Running)).await.unwrap();
        store.delete_checkpoint("a").await.unwrap();
        assert!(store.get_checkpoint("a").await.unwrap().is_none());
    }
}
