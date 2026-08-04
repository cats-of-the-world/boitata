//! Carrying boitata's own run events over ACP.
//!
//! For a boitata orchestrator talking to a boitata agent, the highest-fidelity
//! payload is boitata's own [`AuditEvent`] — it already models exactly what the
//! orchestrator's audit log and web UI consume. So rather than lossily hand-map
//! every event onto an ACP `session/update` variant, we serialize the
//! `AuditEvent` to JSON and carry it as the text of an agent-message chunk; the
//! client parses it straight back. The ACP envelope (initialize / session / prompt
//! / stop-reason) still drives the exchange, so an ACP-native client sees a
//! well-formed stream of message chunks.

use boitata_core::audit::AuditEvent;

/// Serialize an audit event to the JSON text carried in a `session/update` chunk.
pub fn encode(event: &AuditEvent) -> String {
    // AuditEvent is a plain tagged enum; serialization cannot realistically fail.
    serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string())
}

/// Parse an audit event back from a chunk's text, if it is one of ours. Returns
/// `None` for chunk text that isn't a boitata event (e.g. a plain-text agent that
/// isn't boitata), so a foreign ACP agent doesn't break the client.
pub fn decode(text: &str) -> Option<AuditEvent> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_representative_events() {
        let events = vec![
            AuditEvent::RunStarted {
                task: "do it".into(),
                provider: "anthropic".into(),
                model: "glm".into(),
            },
            AuditEvent::ToolCall {
                iteration: 2,
                name: "file_read".into(),
                arguments: "{\"path\":\"a\"}".into(),
                result: "contents".into(),
                is_error: false,
                read_only: true,
            },
            AuditEvent::RunCompleted {
                success: true,
                iterations: 3,
                error: None,
                total_input_tokens: 10,
                total_output_tokens: 20,
            },
        ];
        for ev in &events {
            let text = encode(ev);
            let back = decode(&text).expect("decodes back to an AuditEvent");
            // Compare via their JSON, since AuditEvent isn't PartialEq.
            assert_eq!(encode(&back), text);
        }
    }

    #[test]
    fn decode_ignores_foreign_text() {
        assert!(decode("hello from some other agent").is_none());
        assert!(decode("{\"unrelated\":true}").is_none());
    }
}
