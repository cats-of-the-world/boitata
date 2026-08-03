//! Drive a *real* `boitata_agent::Agent` (backed by a stub provider, no network)
//! through the ACP round-trip, proving the agent loop's own audit events stream
//! back to the client and decode correctly.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use boitata_acp::{AgentOutcome, PromptRunner, run_prompt, serve};
use boitata_agent::{Agent, Task};
use boitata_core::audit::{AuditEvent, AuditSink};
use boitata_core::provider::{
    Chunk, CompletionRequest, CompletionResponse, Provider, ProviderResult,
};
use boitata_core::tools::ToolRegistry;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// A provider that answers every request with a fixed message and no tool calls,
/// so the agent loop completes in one iteration without a network.
struct StubProvider;

#[async_trait]
impl Provider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }
    fn model(&self) -> &str {
        "stub"
    }
    async fn complete(&self, _request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        Ok(CompletionResponse {
            content: Some("done".to_string()),
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

struct RealRunner {
    provider: Arc<dyn Provider>,
}

#[async_trait]
impl PromptRunner for RealRunner {
    async fn run(
        &self,
        prompt: String,
        sink: Arc<dyn AuditSink>,
        cancel: CancellationToken,
    ) -> anyhow::Result<AgentOutcome> {
        let agent = Agent::new(self.provider.clone(), ToolRegistry::new()).with_audit(sink);
        let result = agent.run_with_cancel(Task::new(prompt), cancel).await?;
        Ok(AgentOutcome {
            success: result.success,
            message: result.final_message,
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
async fn real_agent_streams_its_events_over_acp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let runner = Arc::new(RealRunner {
        provider: Arc::new(StubProvider),
    });
    let server = tokio::spawn(async move {
        let _ = serve(listener, runner).await;
    });

    let sink = Arc::new(RecordingSink::default());
    let outcome = run_prompt(
        &addr,
        "hello".into(),
        sink.clone(),
        CancellationToken::new(),
    )
    .await
    .expect("prompt round-trips");
    assert!(outcome.success);

    let events = sink.0.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AuditEvent::RunStarted { .. })),
        "run_started streamed: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AuditEvent::RunCompleted { success: true, .. })),
        "successful run_completed streamed: {events:?}"
    );

    server.abort();
}
