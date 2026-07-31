//! Bridging agent/blueprint audit events to the browser.
//!
//! Every run gets a [`ChannelAuditSink`]: as the agent or executor emits
//! [`AuditEvent`]s, the sink stamps each with a monotonic sequence number,
//! appends it to a replayable history buffer, and broadcasts it to any connected
//! SSE subscribers. Auditing is best-effort, so a full channel or absent
//! subscribers never fails a run.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use boitata_core::audit::{AuditEvent, AuditSink};
use serde::Serialize;
use tokio::sync::broadcast;

/// One live event: a sequence number plus the underlying audit event. The
/// `event` field flattens to the audit event's own `{"event": "...", ...}` shape,
/// so a subscriber sees e.g. `{"seq": 3, "event": "tool_call", ...}`.
#[derive(Debug, Clone, Serialize)]
pub struct RunEvent {
    pub seq: u64,
    #[serde(flatten)]
    pub event: AuditEvent,
}

/// Shared, replayable buffer of everything a run has emitted so far. A late SSE
/// subscriber (or the run-detail endpoint) replays this before going live.
pub type History = Arc<Mutex<Vec<RunEvent>>>;

/// An [`AuditSink`] that fans events out to SSE subscribers and records them for
/// replay. Cloneable handles to `tx`/`history` are shared with the [`RunHandle`]
/// so the API can subscribe and serve history independently of the sink.
pub struct ChannelAuditSink {
    tx: broadcast::Sender<RunEvent>,
    history: History,
    seq: AtomicU64,
}

impl ChannelAuditSink {
    pub fn new(tx: broadcast::Sender<RunEvent>, history: History) -> Self {
        Self {
            tx,
            history,
            seq: AtomicU64::new(0),
        }
    }
}

impl AuditSink for ChannelAuditSink {
    fn record(&self, event: AuditEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let ev = RunEvent { seq, event };
        // Best-effort: a poisoned lock or a send with no live subscribers must
        // never break the run.
        if let Ok(mut history) = self.history.lock() {
            history.push(ev.clone());
        }
        let _ = self.tx.send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_started() -> AuditEvent {
        AuditEvent::RunStarted {
            task: "t".into(),
            provider: "ollama".into(),
            model: "m".into(),
        }
    }

    #[test]
    fn records_to_history_with_monotonic_seq() {
        let (tx, _rx) = broadcast::channel(8);
        let history: History = Arc::new(Mutex::new(Vec::new()));
        let sink = ChannelAuditSink::new(tx, history.clone());

        sink.record(run_started());
        sink.record(run_started());

        let seqs: Vec<u64> = history.lock().unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1]);
    }

    #[tokio::test]
    async fn broadcasts_to_subscribers() {
        let (tx, _keep) = broadcast::channel(8);
        let history: History = Arc::new(Mutex::new(Vec::new()));
        let sink = ChannelAuditSink::new(tx.clone(), history);
        let mut rx = tx.subscribe();

        sink.record(run_started());

        let ev = rx.recv().await.expect("event delivered");
        assert_eq!(ev.seq, 0);
    }

    #[test]
    fn record_without_subscribers_is_ok() {
        let (tx, rx) = broadcast::channel(8);
        drop(rx); // no live subscribers
        let history: History = Arc::new(Mutex::new(Vec::new()));
        let sink = ChannelAuditSink::new(tx, history.clone());

        sink.record(run_started()); // must not panic
        assert_eq!(history.lock().unwrap().len(), 1);
    }
}
