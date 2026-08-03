//! End-to-end ACP round-trip over a loopback TCP socket: a stub agent streams
//! two audit events and completes; the client collects them and the outcome.
//! No LLM or Docker needed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use boitata_acp::{AgentOutcome, PromptRunner, run_prompt, serve};
use boitata_core::audit::{AuditEvent, AuditSink};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Emits a RunStarted + RunCompleted for the prompt, then succeeds.
struct StubRunner;

#[async_trait]
impl PromptRunner for StubRunner {
    async fn run(
        &self,
        prompt: String,
        sink: Arc<dyn AuditSink>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<AgentOutcome> {
        sink.record(AuditEvent::RunStarted {
            task: prompt,
            provider: "stub".into(),
            model: "stub".into(),
        });
        sink.record(AuditEvent::RunCompleted {
            success: true,
            iterations: 1,
            error: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
        });
        Ok(AgentOutcome {
            success: true,
            message: None,
        })
    }
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<AuditEvent>>);

impl AuditSink for RecordingSink {
    fn record(&self, event: AuditEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn round_trips_a_prompt_over_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        let _ = serve(listener, Arc::new(StubRunner)).await;
    });

    let sink = Arc::new(RecordingSink::default());
    let outcome = run_prompt(
        &addr,
        "do the thing".into(),
        sink.clone(),
        CancellationToken::new(),
    )
    .await
    .expect("prompt round-trips");

    assert!(outcome.success, "stub runner reports success");

    // The streamed events arrived and decoded back into AuditEvents, in order.
    let events = sink.0.lock().unwrap();
    assert_eq!(events.len(), 2, "both events streamed: {events:?}");
    assert!(matches!(events[0], AuditEvent::RunStarted { .. }));
    match &events[1] {
        AuditEvent::RunCompleted { success, .. } => assert!(*success),
        other => panic!("expected RunCompleted, got {other:?}"),
    }

    server.abort();
}
