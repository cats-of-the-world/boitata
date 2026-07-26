// Audit logging for agent runs.
//
// Every run emits a stream of structured events (one JSON object per line) so we
// can reconstruct exactly what happened: which model was called, what tool calls
// were made, token usage, and the final outcome. JSONL keeps the log
// human-readable now and trivially loadable into a database later.
//
// Audit failures never abort a run — a write error is logged via `tracing` and
// swallowed, because losing the log is preferable to killing an unattended task.
//
// Secrets are never recorded here: the API key is deliberately excluded from
// every event (only provider/model/base_url-level metadata is captured upstream).

use chrono::Utc;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// A sink that records audit events. Implementors must be cheap to call and must
/// not panic — audit is best-effort and must never break a run.
pub trait AuditSink: Send + Sync {
    fn record(&self, event: AuditEvent);
}

/// One line in the audit log: run/timestamp envelope plus a typed event.
#[derive(Debug, Clone, Serialize)]
struct AuditRecord {
    run_id: String,
    /// RFC 3339 timestamp (UTC).
    timestamp: String,
    #[serde(flatten)]
    event: AuditEvent,
}

/// A single auditable moment in an agent run.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    /// A run began.
    RunStarted {
        task: String,
        provider: String,
        model: String,
    },
    /// The provider returned a completion for one iteration.
    LlmResponse {
        iteration: usize,
        /// Whether the assistant produced any text (vs. tool calls only).
        has_text: bool,
        /// Names of the tools the assistant asked to call this turn.
        tool_calls: Vec<String>,
        input_tokens: Option<usize>,
        output_tokens: Option<usize>,
    },
    /// A tool was executed and produced a result.
    ToolCall {
        iteration: usize,
        name: String,
        arguments: String,
        result: String,
        is_error: bool,
        /// Whether the tool declared itself read-only (see `ToolAnnotations`),
        /// so a reader can distinguish observing from mutating calls.
        read_only: bool,
    },
    /// A tool call was blocked by the permission policy before it ran.
    ToolDenied {
        iteration: usize,
        name: String,
        arguments: String,
        reason: String,
    },
    /// Older turns were summarized into a synopsis to stay within the context
    /// window.
    ContextCompacted {
        iteration: usize,
        /// Estimated prompt tokens before and after compaction.
        tokens_before: usize,
        tokens_after: usize,
        /// Message count before and after compaction.
        messages_before: usize,
        messages_after: usize,
    },
    /// The run finished (successfully or not).
    RunCompleted {
        success: bool,
        iterations: usize,
        error: Option<String>,
        total_input_tokens: usize,
        total_output_tokens: usize,
    },
    /// A blueprint run began.
    BlueprintStarted { blueprint: String, entry: String },
    /// A blueprint node ran and routing chose the next node.
    NodeExecuted {
        step: usize,
        node: String,
        kind: NodeKind,
        /// State status after the node ran.
        status: NodeStatus,
        /// The next node the run moves to (or the END sentinel).
        next: String,
    },
    /// A blueprint run finished.
    BlueprintCompleted {
        steps: usize,
        reason: CompletionReason,
    },
}

/// Which kind of blueprint node ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Agent,
    Tool,
    Script,
}

/// Outcome of a blueprint node.
///
/// This mirrors `blueprint::state::Status` but is a distinct type on purpose:
/// `audit` is a lower-level module that must not depend on `blueprint` (the
/// dependency runs the other way). The executor maps `Status` to `NodeStatus` at
/// the audit boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Ok,
    Failed,
}

/// Why a blueprint run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReason {
    Completed,
    Cancelled,
    StepLimit,
    /// A node or routing step returned an error.
    Error,
}

/// An [`AuditSink`] that appends JSON lines to a file.
pub struct FileAuditLog {
    run_id: String,
    file: Mutex<std::fs::File>,
}

impl FileAuditLog {
    /// Open (or create) the audit log at `path` in append mode, tagging every
    /// event with `run_id`.
    pub fn open(path: &Path, run_id: String) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            run_id,
            file: Mutex::new(file),
        })
    }
}

impl AuditSink for FileAuditLog {
    fn record(&self, event: AuditEvent) {
        let record = AuditRecord {
            run_id: self.run_id.clone(),
            timestamp: Utc::now().to_rfc3339(),
            event,
        };

        let line = match serde_json::to_string(&record) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!("failed to serialize audit event: {e}");
                return;
            }
        };

        match self.file.lock() {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{line}") {
                    tracing::warn!("failed to write audit event: {e}");
                }
            }
            Err(e) => tracing::warn!("audit log mutex poisoned: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_serializes_with_tag() {
        let record = AuditRecord {
            run_id: "run-1".to_string(),
            timestamp: "2026-07-24T00:00:00+00:00".to_string(),
            event: AuditEvent::RunStarted {
                task: "hi".to_string(),
                provider: "openai".to_string(),
                model: "glm-4.6".to_string(),
            },
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""event":"run_started""#));
        assert!(json.contains(r#""run_id":"run-1""#));
        assert!(json.contains(r#""provider":"openai""#));
    }

    #[test]
    fn blueprint_event_enums_serialize_snake_case() {
        let json = serde_json::to_string(&AuditEvent::NodeExecuted {
            step: 1,
            node: "main".to_string(),
            kind: NodeKind::Agent,
            status: NodeStatus::Ok,
            next: "fmt".to_string(),
        })
        .unwrap();
        assert!(json.contains(r#""event":"node_executed""#));
        assert!(json.contains(r#""kind":"agent""#));
        assert!(json.contains(r#""status":"ok""#));

        let done = serde_json::to_string(&AuditEvent::BlueprintCompleted {
            steps: 3,
            reason: CompletionReason::StepLimit,
        })
        .unwrap();
        assert!(done.contains(r#""reason":"step_limit""#));
    }

    #[test]
    fn test_file_audit_log_writes_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = FileAuditLog::open(&path, "run-x".to_string()).unwrap();
        log.record(AuditEvent::RunCompleted {
            success: true,
            iterations: 2,
            error: None,
            total_input_tokens: 10,
            total_output_tokens: 5,
        });
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains(r#""event":"run_completed""#));
        assert!(contents.contains(r#""run_id":"run-x""#));
    }
}
