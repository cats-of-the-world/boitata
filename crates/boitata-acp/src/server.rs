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
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::{PromptRunner, mapping};

/// Monotonic source of session ids (one session per prompt turn here).
static SESSION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Serve `runner` over ACP, accepting one client connection at a time on
/// `listener`. Returns when the listener errors; each accepted connection runs
/// until the client disconnects.
pub async fn serve(listener: TcpListener, runner: Arc<dyn PromptRunner>) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!("ACP client connected: {peer}");
        if let Err(e) = handle_connection(stream, runner.clone()).await {
            tracing::warn!("ACP connection with {peer} ended: {e:#}");
        }
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

    let stop_reason = match run.await {
        Ok(Ok(outcome)) if outcome.success => StopReason::EndTurn,
        Ok(Ok(_)) => StopReason::Refusal,
        Ok(Err(e)) => {
            tracing::warn!("prompt run failed: {e:#}");
            StopReason::Refusal
        }
        Err(e) => {
            tracing::error!("prompt run task panicked: {e}");
            StopReason::Refusal
        }
    };
    responder.respond(PromptResponse::new(stop_reason))
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
