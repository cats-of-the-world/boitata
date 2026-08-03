//! ACP client: drive a remote agent over TCP for one prompt turn.

use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    SessionUpdate, StopReason,
};
use agent_client_protocol::{ByteStreams, Client};
use boitata_core::audit::AuditSink;
use tokio::net::TcpStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::{AgentOutcome, mapping};

/// Connect to the ACP agent at `addr`, run `prompt` to completion, forwarding
/// each streamed [`AuditEvent`](boitata_core::audit::AuditEvent) to `sink`, and
/// return the outcome.
pub async fn run_prompt(
    addr: &str,
    prompt: String,
    sink: Arc<dyn AuditSink>,
    cancel: CancellationToken,
) -> anyhow::Result<AgentOutcome> {
    // Drive the whole turn under the cancellation token. Every step below is a
    // network `.await` with no built-in timeout, so an unresponsive or wedged
    // agent would otherwise hang the caller forever. On cancellation we stop
    // awaiting and return, dropping the session future — which closes the TCP
    // stream — rather than leaking the connection.
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            anyhow::bail!("cancelled while driving the agent at {addr}")
        }
        result = drive_prompt(addr, prompt, sink) => result,
    }
}

/// One prompt turn against the agent at `addr`: connect, initialize, open a
/// session, and run `prompt` to completion, forwarding streamed events to `sink`.
async fn drive_prompt(
    addr: &str,
    prompt: String,
    sink: Arc<dyn AuditSink>,
) -> anyhow::Result<AgentOutcome> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to agent at {addr}: {e}"))?;
    let (read, write) = stream.into_split();
    let transport = ByteStreams::new(write.compat_write(), read.compat());

    let notify_sink = sink.clone();
    let outcome = Client
        .builder()
        .name("boitata-orchestrator")
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                if let SessionUpdate::AgentMessageChunk(chunk) = &notif.update
                    && let ContentBlock::Text(text) = &chunk.content
                    && let Some(event) = mapping::decode(&text.text)
                {
                    notify_sink.record(event);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::from(prompt)],
                ))
                .block_task()
                .await?;
            let message = response
                .meta
                .as_ref()
                .and_then(|m| m.get("message"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Ok(AgentOutcome {
                success: matches!(response.stop_reason, StopReason::EndTurn),
                message,
            })
        })
        .await
        .map_err(|e| anyhow::anyhow!("ACP session error: {e}"))?;

    Ok(outcome)
}
