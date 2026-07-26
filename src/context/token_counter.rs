// Token counting with a real BPE tokenizer (tiktoken `o200k_base`), matching the
// approach goose uses in its `token_counter`. Counts are approximate for any given
// provider (each API frames messages slightly differently) but consistent and good
// enough to decide when the context window is filling up.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

use crate::provider::{Message, MessageContent, ToolDefinition, tool_content_text};

/// Per-message framing overhead (role + delimiters).
const TOKENS_PER_MESSAGE: usize = 4;
/// System prompt framing overhead.
const SYSTEM_OVERHEAD: usize = 4;
/// Priming tokens for the assistant's reply.
const REPLY_PRIMER: usize = 3;

// Structural overhead for tool/function schemas (mirrors goose's constants).
const FUNC_INIT: usize = 7;
const PROP_INIT: usize = 3;
const PROP_KEY: usize = 3;
const FUNC_END: usize = 12;

/// The shared, lazily-initialized tokenizer. Loading the BPE table is not free, so
/// it is built once and reused for the life of the process.
fn bpe() -> &'static CoreBPE {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().expect("load o200k_base tokenizer"))
}

/// Counts tokens for text, tool schemas, and whole chat requests.
#[derive(Debug, Clone, Default)]
pub struct TokenCounter;

impl TokenCounter {
    pub fn new() -> Self {
        Self
    }

    /// Count the tokens in a piece of text.
    pub fn count_tokens(&self, text: &str) -> usize {
        bpe().encode_ordinary(text).len()
    }

    /// Count the tokens the tool definitions add to a request: fixed structural
    /// overhead per function and per property, plus the tokenized names,
    /// descriptions, and schema contents.
    pub fn count_tokens_for_tools(&self, tools: &[ToolDefinition]) -> usize {
        let mut total = 0;
        for tool in tools {
            total += FUNC_INIT;
            total += self.count_tokens(&tool.name);
            total += self.count_tokens(&tool.description);
            if let Some(props) = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
            {
                total += PROP_INIT;
                for (key, value) in props {
                    total += PROP_KEY;
                    total += self.count_tokens(key);
                    total += self.count_tokens(&value.to_string());
                }
            }
            total += FUNC_END;
        }
        total
    }

    /// Estimate the total prompt tokens for a completion request: the system
    /// prompt, every message (with per-message overhead), the tool definitions,
    /// and the reply primer.
    pub fn count_chat_tokens(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> usize {
        let mut total = SYSTEM_OVERHEAD + self.count_tokens(system_prompt);

        for message in messages {
            total += TOKENS_PER_MESSAGE + self.count_tokens(&message_text(&message.content));
        }

        total += self.count_tokens_for_tools(tools);
        total + REPLY_PRIMER
    }
}

/// Flatten a message's content to the text we count. Tool-call arguments and tool
/// results are counted as their serialized form (images collapse to a short
/// placeholder via [`tool_content_text`]).
fn message_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::ToolResults(results) => results
            .iter()
            .map(|r| tool_content_text(&r.content))
            .collect::<Vec<_>>()
            .join("\n"),
        MessageContent::ToolUse { text, tool_calls } => {
            let mut parts = Vec::new();
            if let Some(text) = text {
                parts.push(text.clone());
            }
            for call in tool_calls {
                parts.push(format!("{}({})", call.name, call.arguments));
            }
            parts.join("\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MessageRole, ToolContent, ToolResult};

    #[test]
    fn counts_are_nonzero_and_monotonic() {
        let counter = TokenCounter::new();
        let short = counter.count_tokens("hello");
        let long = counter.count_tokens("hello there, this is a considerably longer sentence");
        assert!(short > 0);
        assert!(long > short);
    }

    #[test]
    fn tool_overhead_grows_with_tool_count() {
        let counter = TokenCounter::new();
        let tool = ToolDefinition {
            name: "search".to_string(),
            description: "Search the codebase".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } }
            }),
        };
        let one = counter.count_tokens_for_tools(std::slice::from_ref(&tool));
        let two = counter.count_tokens_for_tools(&[tool.clone(), tool]);
        assert!(one > 0);
        assert!(two > one);
    }

    #[test]
    fn chat_tokens_include_messages_and_overhead() {
        let counter = TokenCounter::new();
        let messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("do the thing".to_string()),
        }];
        let with_msg = counter.count_chat_tokens("system", &messages, &[]);
        let empty = counter.count_chat_tokens("system", &[], &[]);
        assert!(with_msg > empty);
    }

    #[test]
    fn tool_results_are_counted() {
        let counter = TokenCounter::new();
        let text = message_text(&MessageContent::ToolResults(vec![ToolResult {
            tool_call_id: "t1".to_string(),
            content: vec![ToolContent::text("some output")],
            is_error: Some(false),
        }]));
        assert!(counter.count_tokens(&text) > 0);
    }
}
