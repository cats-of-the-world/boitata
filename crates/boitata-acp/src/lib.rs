//! Agent Client Protocol (ACP) integration for boitata.
//!
//! [`serve`] exposes an agent over ACP on a TCP socket; [`run_prompt`] is the
//! matching client the orchestrator uses to drive an agent that runs elsewhere
//! (eventually inside a sandbox/VM). boitata's own [`AuditEvent`]s ride inside the
//! protocol's message chunks (see [`mapping`]), so the orchestrator's audit/SSE
//! stream works unchanged.

use std::sync::Arc;

use async_trait::async_trait;
use boitata_core::audit::AuditSink;
use tokio_util::sync::CancellationToken;

pub mod mapping;

mod client;
mod server;

pub use client::run_prompt;
pub use server::serve;

/// The outcome of a prompt turn, returned to the client.
#[derive(Debug, Clone, Default)]
pub struct AgentOutcome {
    pub success: bool,
    pub message: Option<String>,
}

/// Runs a single prompt turn for the ACP server: execute the task, forwarding
/// each [`AuditEvent`](boitata_core::audit::AuditEvent) to `sink` as it happens,
/// and return the outcome. Implemented by `boitata-agent` (build an `Agent` and
/// run it); kept as a trait so this crate doesn't depend on the agent/provider.
#[async_trait]
pub trait PromptRunner: Send + Sync + 'static {
    async fn run(
        &self,
        prompt: String,
        sink: Arc<dyn AuditSink>,
        cancel: CancellationToken,
    ) -> anyhow::Result<AgentOutcome>;
}
