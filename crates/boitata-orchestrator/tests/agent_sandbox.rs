//! Full `agent_sandbox` node path without Docker: a fake `Sandbox` (no-op exec,
//! endpoint → a loopback address) points at a real ACP server (stub runner). The
//! executor runs a one-node blueprint; we assert the agent's streamed events
//! reach the run's audit sink, the node output is the agent's final message, and
//! routing follows the outcome.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use boitata_acp::{AgentOutcome, PromptRunner, serve};
use boitata_core::audit::{AuditEvent, AuditSink};
use boitata_core::provider::{
    Chunk, CompletionRequest, CompletionResponse, Provider, ProviderResult,
};
use boitata_core::tools::ToolRegistry;
use boitata_orchestrator::{Executor, Sandbox, Status, load};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// The in-"sandbox" agent: streams one event and returns a final message.
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
            message: Some("the agent did the thing".into()),
        })
    }
}

/// A sandbox backend that doesn't touch Docker: `exec` is a no-op and `endpoint`
/// hands back the loopback address of the ACP server started by the test.
struct FakeSandbox {
    addr: String,
}
#[async_trait]
impl Sandbox for FakeSandbox {
    async fn provision(
        &self,
        image: &str,
        _env: &[(String, String)],
        _c: &CancellationToken,
    ) -> anyhow::Result<String> {
        Ok(format!("fake-{image}"))
    }
    async fn exec(
        &self,
        _id: &str,
        _argv: Vec<String>,
        _workdir: Option<&str>,
        _c: &CancellationToken,
    ) -> anyhow::Result<(i64, String)> {
        Ok((0, String::new()))
    }
    async fn endpoint(&self, _id: &str, _port: u16) -> anyhow::Result<String> {
        Ok(self.addr.clone())
    }
    async fn destroy(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Minimal provider so `Executor::new` has one; the agent_sandbox node never
/// calls it (it delegates over ACP).
struct DummyProvider;
#[async_trait]
impl Provider for DummyProvider {
    fn name(&self) -> &str {
        "dummy"
    }
    fn model(&self) -> &str {
        "dummy"
    }
    async fn complete(&self, _r: CompletionRequest) -> ProviderResult<CompletionResponse> {
        Ok(CompletionResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: Some("stop".into()),
        })
    }
    async fn stream_complete(
        &self,
        _r: CompletionRequest,
    ) -> ProviderResult<tokio_stream::wrappers::ReceiverStream<ProviderResult<Chunk>>> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
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
async fn agent_sandbox_node_runs_the_agent_over_acp() {
    // A real ACP server (stub runner) on a loopback port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        let _ = serve(listener, Arc::new(StubRunner)).await;
    });

    // A one-node blueprint using the agent_sandbox node.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bp.yaml");
    std::fs::write(
        &path,
        r#"
name: t
entry: run
nodes:
  run: {type: agent_sandbox, container: "box", prompt: "{task}"}
edges:
  - {from: run, to: END}
"#,
    )
    .unwrap();
    let graph = load(path.to_str().unwrap()).unwrap();

    let sink = Arc::new(RecordingSink::default());
    let executor = Executor::new(Arc::new(DummyProvider), ToolRegistry::new())
        .with_audit(sink.clone())
        .with_sandbox(Arc::new(FakeSandbox { addr }));

    let state = executor
        .run_with_cancel(&graph, "do it".into(), CancellationToken::new())
        .await
        .expect("blueprint runs");

    // Routing followed the agent's success, and the node output is its message.
    assert_eq!(state.status, Some(Status::Ok));
    let transcript: Vec<_> = state.transcript().collect();
    assert_eq!(transcript, vec![("run", "the agent did the thing")]);

    // The agent's events were forwarded into the run's audit stream.
    let events = sink.0.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AuditEvent::RunStarted { provider, .. } if provider == "stub")),
        "the in-sandbox agent's run_started was forwarded: {events:?}"
    );

    server.abort();
}
