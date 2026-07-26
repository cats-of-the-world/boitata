// Compaction: when the conversation approaches the model's context window, the
// oldest turns are summarized into a single synopsis message so the run can
// continue instead of overflowing. This mirrors goose's summarization-based
// context management.
//
// The mechanics here are deterministic and testable; the actual model call that
// produces the summary is driven by the agent, which owns the provider.

use super::{Context, ContextContent, ContextMessage};
use crate::provider::{MessageRole, tool_content_text};

/// How many of the most recent messages to keep verbatim when summarizing. The
/// cutoff is snapped to a turn boundary, so the number kept is approximate.
pub const KEEP_RECENT_MESSAGES: usize = 6;

/// Prefix on the synthetic user message that carries the summary back into the
/// conversation.
const SUMMARY_PREFIX: &str = "Summary of the earlier conversation (older turns were \
    compacted to stay within the context window):\n\n";

/// System prompt for the summarization call the agent makes.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are compacting a coding agent's \
    conversation so it fits within the model's context window. Summarize the \
    conversation below, preserving everything needed to continue the task: the \
    original request, decisions made, files and code inspected or changed, command \
    and tool results that matter, and any unresolved problems or next steps. Be \
    concise but do not omit load-bearing details. Respond with the summary only.";

/// Whether the current usage has crossed the compaction threshold. A `threshold`
/// of `0.0` (or a non-positive `limit`) disables compaction.
pub fn needs_compaction(used: usize, limit: usize, threshold: f32) -> bool {
    if threshold <= 0.0 || limit == 0 {
        return false;
    }
    used as f32 >= limit as f32 * threshold
}

/// Choose where to split the history: messages `[0..cutoff)` are summarized and
/// `[cutoff..]` are kept verbatim. Returns `None` when there is nothing worth
/// summarizing.
///
/// The cutoff always lands on an assistant message so that (a) the summarized
/// prefix never orphans a tool result from its `tool_use`, and (b) the synthetic
/// user message carrying the summary is followed by an assistant turn, keeping the
/// user/assistant alternation that strict providers (e.g. Anthropic) require.
pub fn pick_cutoff(ctx: &Context, keep_recent: usize) -> Option<usize> {
    let msgs = &ctx.messages;
    let len = msgs.len();
    if len <= keep_recent {
        return None;
    }

    // Start from the desired suffix length, then advance to the next assistant
    // message so the kept suffix begins on a clean turn boundary.
    let mut cutoff = len - keep_recent;
    while cutoff < len && !matches!(msgs[cutoff].role, MessageRole::Assistant) {
        cutoff += 1;
    }

    // Need at least the task plus one more message in the prefix to be worth it,
    // and a non-empty kept suffix.
    if cutoff < 2 || cutoff >= len {
        return None;
    }
    Some(cutoff)
}

/// Render the messages in `[0..cutoff)` to plain text for the summarization call.
pub fn render_for_summary(ctx: &Context, cutoff: usize) -> String {
    ctx.messages[..cutoff]
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_message(message: &ContextMessage) -> String {
    let (label, body) = match &message.content {
        ContextContent::Text(text) => (role_label(&message.role), text.clone()),
        ContextContent::ToolResults(results) => {
            let body = results
                .iter()
                .map(|r| tool_content_text(&r.content))
                .collect::<Vec<_>>()
                .join("\n");
            ("Tool results", body)
        }
        ContextContent::ToolUse { text, tool_calls } => {
            let mut parts = Vec::new();
            if let Some(text) = text {
                if !text.is_empty() {
                    parts.push(text.clone());
                }
            }
            for call in tool_calls {
                parts.push(format!("[calls {}({})]", call.name, call.arguments));
            }
            ("Assistant", parts.join("\n"))
        }
    };
    format!("{label}: {body}")
}

fn role_label(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "System",
        MessageRole::User => "User",
        MessageRole::Assistant => "Assistant",
        MessageRole::Tool => "Tool results",
    }
}

/// Replace messages `[0..cutoff)` with a single user message carrying `summary`.
/// The kept suffix (which begins on an assistant turn, see [`pick_cutoff`]) is left
/// intact, so tool-call pairing and role alternation remain valid.
pub fn apply_summary(ctx: &mut Context, cutoff: usize, summary: String) {
    let summary_message = ContextMessage {
        role: MessageRole::User,
        content: ContextContent::Text(format!("{SUMMARY_PREFIX}{summary}")),
    };
    ctx.messages
        .splice(0..cutoff, std::iter::once(summary_message));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn tool_use(name: &str) -> ToolCall {
        ToolCall {
            id: format!("id-{name}"),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    /// task, then N (assistant tool_use, tool result) pairs.
    fn conversation(pairs: usize) -> Context {
        let mut ctx = Context::new();
        ctx.add_user_message("original task");
        for i in 0..pairs {
            ctx.add_assistant_tool_use(None, vec![tool_use(&format!("t{i}"))]);
            ctx.add_tool_result(&format!("id-t{i}"), vec![], false);
        }
        ctx
    }

    #[test]
    fn needs_compaction_threshold() {
        assert!(!needs_compaction(100, 1000, 0.8)); // 10% used
        assert!(needs_compaction(800, 1000, 0.8)); // exactly at threshold
        assert!(needs_compaction(900, 1000, 0.8));
        assert!(!needs_compaction(900, 1000, 0.0)); // disabled
        assert!(!needs_compaction(900, 0, 0.8)); // unknown limit
    }

    #[test]
    fn cutoff_lands_on_assistant_and_keeps_suffix() {
        let ctx = conversation(6); // 1 + 12 = 13 messages
        let cutoff = pick_cutoff(&ctx, KEEP_RECENT_MESSAGES).expect("should compact");
        // The kept suffix must start with an assistant turn, never an orphaned
        // tool result.
        assert!(matches!(ctx.messages[cutoff].role, MessageRole::Assistant));
        assert!(cutoff >= 2 && cutoff < ctx.len());
    }

    #[test]
    fn no_cutoff_for_short_history() {
        let ctx = conversation(1); // 3 messages, below keep-recent
        assert!(pick_cutoff(&ctx, KEEP_RECENT_MESSAGES).is_none());
    }

    #[test]
    fn apply_summary_preserves_pairing() {
        let mut ctx = conversation(6);
        let cutoff = pick_cutoff(&ctx, KEEP_RECENT_MESSAGES).unwrap();
        let kept = ctx.len() - cutoff;
        apply_summary(&mut ctx, cutoff, "a synopsis".to_string());

        // One synthetic user message replaced the whole prefix.
        assert_eq!(ctx.len(), kept + 1);
        assert!(matches!(ctx.messages[0].role, MessageRole::User));
        assert!(matches!(
            &ctx.messages[0].content,
            ContextContent::Text(t) if t.contains("a synopsis")
        ));
        // The message right after the summary is an assistant turn: alternation
        // and tool_use/tool_result pairing are intact.
        assert!(matches!(ctx.messages[1].role, MessageRole::Assistant));

        // Every tool-results message is preceded by an assistant tool_use.
        for i in 0..ctx.len() {
            if matches!(ctx.messages[i].role, MessageRole::Tool) {
                assert!(i > 0);
                assert!(matches!(
                    &ctx.messages[i - 1].content,
                    ContextContent::ToolUse { .. }
                ));
            }
        }
    }
}
