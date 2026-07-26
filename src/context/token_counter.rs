// Token counting with a real BPE tokenizer (tiktoken `o200k_base`), matching the
// approach goose uses in its `token_counter`. Counts are approximate for any given
// provider (each API frames messages slightly differently) but consistent and good
// enough to decide when the context window is filling up.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

use super::{ContextContent, ContextMessage};
use crate::provider::{ToolDefinition, tool_content_text};

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
/// it is built once and reused for the life of the process. If it ever fails to
/// load we cache the failure (`None`) and fall back to a rough estimate rather than
/// panicking — token counting is on the best-effort compaction path.
fn bpe() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| match tiktoken_rs::o200k_base() {
        Ok(bpe) => Some(bpe),
        Err(e) => {
            tracing::warn!(
                "failed to load o200k_base tokenizer ({e}); using a rough token estimate"
            );
            None
        }
    })
    .as_ref()
}

/// Counts tokens for text, tool schemas, and whole conversations.
#[derive(Debug, Clone, Default)]
pub struct TokenCounter;

impl TokenCounter {
    pub fn new() -> Self {
        Self
    }

    /// Count the tokens in a piece of text. Synchronous and CPU-bound; callers on
    /// the async path keep it cheap by counting incrementally over borrowed
    /// content (see [`Self::count_context_tokens`]) rather than re-tokenizing
    /// whole cloned histories.
    pub fn count_tokens(&self, text: &str) -> usize {
        match bpe() {
            Some(bpe) => bpe.encode_ordinary(text).len(),
            // Fallback (~4 chars/token) so the agent keeps running if the
            // tokenizer failed to load.
            None => text.len().div_ceil(4),
        }
    }

    /// Count the tokens the tool definitions add to a request: fixed structural
    /// overhead per function and per property, plus the tokenized names,
    /// descriptions, and schema contents. The result is constant for a run, so
    /// callers compute it once and reuse it (see [`Self::count_context_tokens`]).
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

    /// Estimate the total prompt tokens for a conversation: the system prompt,
    /// every message (with per-message overhead), the tool definitions, and the
    /// reply primer.
    ///
    /// Counts directly over the context's internal messages to avoid cloning the
    /// whole history each call. `tool_tokens` is the (run-constant) tool schema
    /// count from [`Self::count_tokens_for_tools`], passed in so it isn't
    /// recomputed every iteration.
    pub(super) fn count_context_tokens(
        &self,
        system_prompt: &str,
        messages: &[ContextMessage],
        tool_tokens: usize,
    ) -> usize {
        let mut total = SYSTEM_OVERHEAD + self.count_tokens(system_prompt);
        for message in messages {
            total += TOKENS_PER_MESSAGE + self.count_content(&message.content);
        }
        total + tool_tokens + REPLY_PRIMER
    }

    /// Count the tokens in one message's content, summing over its parts without
    /// building an intermediate joined string. Tool-call arguments and tool
    /// results are counted as their serialized form (images collapse to a short
    /// placeholder via [`tool_content_text`]).
    fn count_content(&self, content: &ContextContent) -> usize {
        match content {
            ContextContent::Text(text) => self.count_tokens(text),
            ContextContent::ToolResults(results) => results
                .iter()
                .map(|r| self.count_tokens(&tool_content_text(&r.content)))
                .sum(),
            ContextContent::ToolUse { text, tool_calls } => {
                let mut total = text.as_deref().map(|t| self.count_tokens(t)).unwrap_or(0);
                for call in tool_calls {
                    total += self.count_tokens(&call.name);
                    total += self.count_tokens(&call.arguments.to_string());
                }
                total
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
