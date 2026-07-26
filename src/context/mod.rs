// Context module: conversation state plus token counting and compaction.
//
// `Context` owns the message history for a run. When the history grows toward the
// model's context window, [`compact`] summarizes the oldest turns into a single
// synopsis message so long runs survive instead of overflowing (mirroring goose's
// `context_mgmt`). [`TokenCounter`] measures usage with a real tokenizer.

mod compact;
mod token_counter;

pub use compact::{
    KEEP_RECENT_MESSAGES, SUMMARIZATION_SYSTEM_PROMPT, apply_summary, needs_compaction,
    pick_cutoff, render_for_summary,
};
pub use token_counter::TokenCounter;

use crate::provider::{Message, MessageContent, MessageRole, ToolCall, ToolContent, ToolResult};

/// Conversation context
#[derive(Debug, Clone)]
pub struct Context {
    messages: Vec<ContextMessage>,
    system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
struct ContextMessage {
    role: MessageRole,
    content: ContextContent,
}

#[derive(Debug, Clone)]
enum ContextContent {
    Text(String),
    ToolResults(Vec<ToolResult>),
    /// An assistant turn that requested tool calls, with any accompanying text.
    ToolUse {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
}

impl Context {
    /// Create a new context
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            system_prompt: None,
        }
    }

    /// Set the system prompt
    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = Some(prompt);
    }

    /// Add a user message
    pub fn add_user_message(&mut self, content: &str) {
        self.messages.push(ContextMessage {
            role: MessageRole::User,
            content: ContextContent::Text(content.to_string()),
        });
    }

    /// Add an assistant message
    pub fn add_assistant_message(&mut self, content: &str) {
        self.messages.push(ContextMessage {
            role: MessageRole::Assistant,
            content: ContextContent::Text(content.to_string()),
        });
    }

    /// Record an assistant turn that requested tool calls, along with any text
    /// the assistant produced. This must be added before the corresponding tool
    /// results so the provider sees a matching `tool_use` for each result.
    pub fn add_assistant_tool_use(&mut self, text: Option<String>, tool_calls: Vec<ToolCall>) {
        self.messages.push(ContextMessage {
            role: MessageRole::Assistant,
            content: ContextContent::ToolUse { text, tool_calls },
        });
    }

    /// Add a tool result. `content` is the tool's structured output (text and/or
    /// images), carried through so providers that support image tool results can
    /// pass them to the model.
    pub fn add_tool_result(
        &mut self,
        tool_call_id: &str,
        content: Vec<ToolContent>,
        is_error: bool,
    ) {
        // Find or create a tool results message
        let last_is_tool_results = self
            .messages
            .last()
            .map(|m| matches!(m.role, MessageRole::Tool))
            .unwrap_or(false);

        if last_is_tool_results {
            // Add to existing tool results
            if let Some(ContextMessage {
                content: ContextContent::ToolResults(results),
                ..
            }) = self.messages.last_mut()
            {
                results.push(ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    content,
                    is_error: Some(is_error),
                });
            }
        } else {
            // Create new tool results message
            self.messages.push(ContextMessage {
                role: MessageRole::Tool,
                content: ContextContent::ToolResults(vec![ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    content,
                    is_error: Some(is_error),
                }]),
            });
        }
    }

    /// Get all messages as provider messages
    pub fn to_messages(&self) -> Vec<Message> {
        self.messages
            .iter()
            .map(|m| Message {
                role: m.role.clone(),
                content: match &m.content {
                    ContextContent::Text(text) => MessageContent::Text(text.clone()),
                    ContextContent::ToolResults(results) => {
                        MessageContent::ToolResults(results.clone())
                    }
                    ContextContent::ToolUse { text, tool_calls } => MessageContent::ToolUse {
                        text: text.clone(),
                        tool_calls: tool_calls.clone(),
                    },
                },
            })
            .collect()
    }

    /// Estimate the tokens this context would occupy in a completion request,
    /// including the system prompt and tool definitions. Used to decide when to
    /// compact (see [`needs_compaction`]).
    ///
    /// `tool_tokens` is the run-constant tool-schema count from
    /// [`TokenCounter::count_tokens_for_tools`]; the caller computes it once and
    /// passes it in so it isn't recomputed every iteration. Counting happens over
    /// the internal messages directly, avoiding a clone of the whole history.
    pub fn token_count(&self, counter: &TokenCounter, tool_tokens: usize) -> usize {
        let system = self.system_prompt.as_deref().unwrap_or("");
        counter.count_context_tokens(system, &self.messages, tool_tokens)
    }

    /// Get the number of messages
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Check if the context is empty
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Clear all messages
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_new() {
        let ctx = Context::new();
        assert!(ctx.is_empty());
        assert_eq!(ctx.len(), 0);
    }

    #[test]
    fn test_add_user_message() {
        let mut ctx = Context::new();
        ctx.add_user_message("Hello");
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn test_add_tool_result() {
        let mut ctx = Context::new();
        ctx.add_tool_result("tool-1", vec![ToolContent::text("result")], false);
        assert_eq!(ctx.len(), 1);

        // Add another tool result - should be grouped
        ctx.add_tool_result("tool-2", vec![ToolContent::text("result2")], false);
        assert_eq!(ctx.len(), 1); // Still 1 message with 2 results
    }

    #[test]
    fn test_clear() {
        let mut ctx = Context::new();
        ctx.add_user_message("Hello");
        ctx.clear();
        assert!(ctx.is_empty());
    }

    #[test]
    fn token_count_grows_with_history_and_tools() {
        let counter = TokenCounter::new();
        let mut ctx = Context::new();
        ctx.set_system_prompt("system".to_string());

        let base = ctx.token_count(&counter, 0);
        ctx.add_user_message("do the thing");
        let with_msg = ctx.token_count(&counter, 0);
        assert!(with_msg > base, "adding a message should raise the count");

        // The tool-schema tokens are added on top.
        assert!(ctx.token_count(&counter, 100) > with_msg);
    }

    #[test]
    fn token_count_includes_tool_results() {
        let counter = TokenCounter::new();
        let mut ctx = Context::new();
        let before = ctx.token_count(&counter, 0);
        ctx.add_tool_result("t1", vec![ToolContent::text("some tool output")], false);
        assert!(ctx.token_count(&counter, 0) > before);
    }
}
