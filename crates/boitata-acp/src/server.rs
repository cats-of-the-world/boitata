//! ACP agent server: expose a [`PromptRunner`] over a TCP socket.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::{Agent, ByteStreams};
use boitata_core::audit::{AuditEvent, AuditSink};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::{PromptRunner, mapping};

/// Monotonic source of session ids (one session per prompt turn here).
static SESSION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Cap on simultaneous ACP connections. Each holds a socket and an audit
/// channel; bounding them stops a flood of held-open peers from exhausting the
/// host. Waiting peers queue in the OS accept backlog.
const MAX_CONNECTIONS: usize = 64;

/// Serve `runner` over ACP. Each accepted connection runs on its own task so a
/// single slow/stuck peer can't head-of-line block every other client; a bounded
/// semaphore caps concurrency (backpressure pauses `accept` when full). Returns
/// when the listener errors.
pub async fn serve(listener: TcpListener, runner: Arc<dyn PromptRunner>) -> anyhow::Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, peer) = listener.accept().await?;
        // Backpressure: when at capacity, await a permit before accepting more,
        // so live (in-flight) connections — not spawned tasks — are bounded.
        let permit = slots.clone().acquire_owned().await?;
        let runner = runner.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection task ends
            tracing::info!("ACP client connected: {peer}");
            if let Err(e) = handle_connection(stream, runner).await {
                tracing::warn!("ACP connection with {peer} ended: {e:#}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    runner: Arc<dyn PromptRunner>,
) -> anyhow::Result<()> {
    let (read, write) = stream.into_split();
    let transport = ByteStreams::new(write.compat_write(), read.compat());

    let prompt_runner = runner.clone();
    Agent
        .builder()
        .name("boitata-agent")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: NewSessionRequest, responder, _cx| {
                let n = SESSION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = SessionId::new(format!("session-{n}"));
                responder.respond(NewSessionResponse::new(id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |req: PromptRequest, responder, cx| {
                let runner = prompt_runner.clone();
                Box::pin(run_prompt_turn(req, responder, cx, runner))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await
        .map_err(|e| anyhow::anyhow!("ACP connection error: {e}"))
}

/// The `session/prompt` handler: run the task, streaming each audit event back as
/// an `agent_message_chunk`, then respond with a stop reason.
async fn run_prompt_turn(
    req: PromptRequest,
    responder: agent_client_protocol::Responder<PromptResponse>,
    cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
    runner: Arc<dyn PromptRunner>,
) -> Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.clone();
    let prompt = blocks_to_text(&req.prompt);

    // Bridge the sync AuditSink to async notification sends via a channel.
    let (tx, mut rx) = mpsc::unbounded_channel::<AuditEvent>();
    let sink = Arc::new(ChannelSink { tx });
    let cancel = CancellationToken::new();

    // Run the agent concurrently with draining its events. When the run finishes
    // it drops the sink, closing the channel and ending the drain loop.
    let run = tokio::spawn({
        let cancel = cancel.clone();
        async move { runner.run(prompt, sink, cancel).await }
    });

    while let Some(event) = rx.recv().await {
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
            mapping::encode(&event),
        )));
        // A send failure means the client disconnected. Cancel the run so the
        // agent stops promptly (instead of running to completion streaming into
        // the void) and stop draining — nothing is listening anymore.
        if cx
            .send_notification(SessionNotification::new(session_id.clone(), update))
            .is_err()
        {
            tracing::info!("ACP client disconnected mid-prompt; cancelling the run");
            cancel.cancel();
            break;
        }
    }

    let (stop_reason, message) = match run.await {
        Ok(Ok(outcome)) => (
            if outcome.success {
                StopReason::EndTurn
            } else {
                StopReason::Refusal
            },
            outcome.message,
        ),
        Ok(Err(e)) => {
            tracing::warn!("prompt run failed: {e:#}");
            (StopReason::Refusal, Some(format!("{e:#}")))
        }
        Err(e) => {
            tracing::error!("prompt run task panicked: {e}");
            (StopReason::Refusal, None)
        }
    };

    // Carry the agent's final message in the response `_meta` so the client can
    // use it as the node's output (the audit-event stream doesn't include it).
    let mut response = PromptResponse::new(stop_reason);
    if let Some(message) = message {
        let mut meta = serde_json::Map::new();
        meta.insert("message".to_string(), serde_json::Value::String(message));
        response = response.meta(meta);
    }
    responder.respond(response)
}

/// Concatenate the text of a prompt's content blocks.
fn blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            out.push_str(&text.text);
        }
    }
    out
}

/// An [`AuditSink`] that forwards each event to the prompt handler's channel.
struct ChannelSink {
    tx: mpsc::UnboundedSender<AuditEvent>,
}

impl AuditSink for ChannelSink {
    fn record(&self, event: AuditEvent) {
        let _ = self.tx.send(event);
    }
}
