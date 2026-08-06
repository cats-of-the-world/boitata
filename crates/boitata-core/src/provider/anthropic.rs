// Anthropic Claude provider implementation
//
// API reference:
// - Messages API:      https://docs.claude.com/en/api/messages
// - Versioning header:  https://docs.claude.com/en/api/versioning
// - Tool use:          https://docs.claude.com/en/docs/agents-and-tools/tool-use/overview
// - Errors:            https://docs.claude.com/en/api/errors

use super::{
    Chunk, CompletionRequest, CompletionResponse, MessageContent, MessageRole, Provider,
    ProviderError, ProviderResult, ToolCall, ToolContent, ToolDefinition, Usage, http_client,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

/// Value for the required `anthropic-version` request header.
///
/// See <https://docs.claude.com/en/api/versioning>.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Anthropic Claude provider
#[derive(Clone)]
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    max_tokens: usize,
    base_url: String,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the API key (mirrors `Config`'s redaction discipline).
        f.debug_struct("AnthropicProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Create a new Anthropic provider
    pub fn new(api_key: String, model: String) -> Self {
        Self::with_config(api_key, model, None)
    }

    /// Create with custom base URL
    pub fn with_config(api_key: String, model: String, base_url: Option<String>) -> Self {
        Self {
            client: http_client(),
            api_key,
            model,
            // Output (generation) token budget, not the context window size.
            max_tokens: 8192,
            base_url: base_url
                .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".to_string()),
        }
    }

    /// Set the maximum tokens
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn build_request(&self, request: CompletionRequest) -> AnthropicRequest {
        let system = request.system;
        let messages: Vec<AnthropicMessage> = request
            .messages
            .into_iter()
            .filter_map(|m| {
                if m.role == MessageRole::System {
                    None
                } else {
                    Some(AnthropicMessage {
                        role: match m.role {
                            MessageRole::User => "user".to_string(),
                            MessageRole::Assistant => "assistant".to_string(),
                            MessageRole::Tool => "user".to_string(), // Tool results as user messages
                            MessageRole::System => unreachable!(),
                        },
                        content: m.content.into(),
                    })
                }
            })
            .collect();

        AnthropicRequest {
            model: self.model.clone(),
            messages,
            system,
            tools: request.tools,
            max_tokens: request.max_tokens.unwrap_or(4096).min(self.max_tokens),
            temperature: request.temperature,
            stream: false,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    fn context_limit(&self) -> usize {
        // Claude models expose a 200k-token context window.
        200_000
    }

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let anthropic_request = self.build_request(request);

        // POST /v1/messages with the three required headers.
        // See https://docs.claude.com/en/api/messages
        let response = self
            .client
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .json(&anthropic_request)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        if !status.is_success() {
            return Err(parse_anthropic_error(status.as_u16(), &body));
        }

        let anthropic_response: AnthropicResponse = serde_json::from_str(&body)
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        Ok(convert_anthropic_response(anthropic_response))
    }

    async fn stream_complete(
        &self,
        request: CompletionRequest,
    ) -> ProviderResult<ReceiverStream<ProviderResult<Chunk>>> {
        let mut anthropic_request = self.build_request(request);
        anthropic_request.stream = true;

        // POST /v1/messages with `stream: true`; the body is a sequence of SSE
        // events. See https://docs.claude.com/en/docs/build-with-claude/streaming
        let response = self
            .client
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .json(&anthropic_request)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;
            return Err(parse_anthropic_error(status.as_u16(), &body));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            let mut byte_stream = Box::pin(response.bytes_stream());
            // Accumulate raw bytes: a network chunk can split both an SSE line
            // and a multi-byte UTF-8 character, so we buffer bytes and only
            // decode once we hold a complete line.
            let mut buffer: Vec<u8> = Vec::new();
            // A tool_use block streams its input incrementally: (id, name, partial JSON).
            let mut current_tool: Option<(String, String, String)> = None;

            while let Some(item) = byte_stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx
                            .send(Err(ProviderError::RequestFailed(e.to_string())))
                            .await;
                        return;
                    }
                };
                buffer.extend_from_slice(&bytes);

                while let Some(newline) = buffer.iter().position(|&b| b == b'\n') {
                    // A full line (with all its bytes) is now buffered, so any
                    // multi-byte character within it is complete.
                    let line_bytes: Vec<u8> = buffer.drain(..=newline).collect();
                    let line = String::from_utf8_lossy(&line_bytes);
                    let line = line.trim();
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };

                    match event.get("type").and_then(|t| t.as_str()) {
                        Some("content_block_start") => {
                            let cb = &event["content_block"];
                            if cb.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                let id = cb
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                let name = cb
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                current_tool = Some((id, name, String::new()));
                            }
                        }
                        Some("content_block_delta") => {
                            let delta = &event["delta"];
                            match delta.get("type").and_then(|t| t.as_str()) {
                                Some("text_delta") => {
                                    if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                        let chunk = Chunk {
                                            content: Some(text.to_string()),
                                            tool_calls: None,
                                            finish_reason: None,
                                        };
                                        if tx.send(Ok(chunk)).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let (Some(tool), Some(partial)) = (
                                        current_tool.as_mut(),
                                        delta.get("partial_json").and_then(|v| v.as_str()),
                                    ) {
                                        tool.2.push_str(partial);
                                    }
                                }
                                _ => {}
                            }
                        }
                        Some("content_block_stop") => {
                            if let Some((id, name, json)) = current_tool.take() {
                                let arguments = serde_json::from_str(&json)
                                    .unwrap_or_else(|_| serde_json::json!({}));
                                let chunk = Chunk {
                                    content: None,
                                    tool_calls: Some(vec![ToolCall {
                                        id,
                                        name,
                                        arguments,
                                    }]),
                                    finish_reason: None,
                                };
                                if tx.send(Ok(chunk)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some("message_delta") => {
                            if let Some(reason) =
                                event["delta"].get("stop_reason").and_then(|v| v.as_str())
                            {
                                let chunk = Chunk {
                                    content: None,
                                    tool_calls: None,
                                    finish_reason: Some(reason.to_string()),
                                };
                                if tx.send(Ok(chunk)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some("message_stop") => return,
                        Some("error") => {
                            let msg = event["error"]["message"]
                                .as_str()
                                .unwrap_or("stream error")
                                .to_string();
                            let _ = tx.send(Err(ProviderError::RequestFailed(msg))).await;
                            return;
                        }
                        _ => {}
                    }
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }
}

// Types for Anthropic API

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    max_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text {
        #[serde(rename = "type")]
        content_type: String,
        text: String,
    },
    ToolUse {
        #[serde(rename = "type")]
        content_type: String,
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        #[serde(rename = "type")]
        content_type: String,
        tool_use_id: String,
        // Anthropic accepts an array of text/image blocks as tool_result content,
        // so tool output (which may include images) is preserved rather than
        // flattened to a string.
        content: Vec<AnthropicToolResultBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// A block inside a `tool_result`: either text or a base64 image. See
/// <https://docs.claude.com/en/docs/build-with-claude/vision>.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolResultBlock {
    Text { text: String },
    Image { source: AnthropicImageSource },
}

#[derive(Debug, Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

/// Map Boitata tool content into Anthropic `tool_result` blocks, preserving
/// images as base64 sources.
fn tool_result_blocks(content: Vec<ToolContent>) -> Vec<AnthropicToolResultBlock> {
    content
        .into_iter()
        .map(|item| match item {
            ToolContent::Text { text } => AnthropicToolResultBlock::Text { text },
            ToolContent::Image { mime_type, data } => AnthropicToolResultBlock::Image {
                source: AnthropicImageSource {
                    source_type: "base64".to_string(),
                    media_type: mime_type,
                    data,
                },
            },
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    content: Vec<AnthropicContent>,
}

impl From<MessageContent> for Vec<AnthropicContent> {
    fn from(content: MessageContent) -> Self {
        match content {
            MessageContent::Text(text) => vec![AnthropicContent::Text {
                content_type: "text".to_string(),
                text,
            }],
            MessageContent::ToolResults(results) => results
                .into_iter()
                .map(|r| AnthropicContent::ToolResult {
                    content_type: "tool_result".to_string(),
                    tool_use_id: r.tool_call_id,
                    content: tool_result_blocks(r.content),
                    is_error: r.is_error,
                })
                .collect(),
            MessageContent::ToolUse { text, tool_calls } => {
                let mut blocks = Vec::new();
                if let Some(text) = text {
                    if !text.is_empty() {
                        blocks.push(AnthropicContent::Text {
                            content_type: "text".to_string(),
                            text,
                        });
                    }
                }
                for call in tool_calls {
                    blocks.push(AnthropicContent::ToolUse {
                        content_type: "tool_use".to_string(),
                        id: call.id,
                        name: call.name,
                        input: call.arguments,
                    });
                }
                blocks
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    id: String,
    content: Vec<AnthropicResponseContent>,
    model: String,
    role: String,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicResponseContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: usize,
    output_tokens: usize,
    cache_creation_input_tokens: Option<usize>,
    cache_read_input_tokens: Option<usize>,
}

fn convert_anthropic_response(response: AnthropicResponse) -> CompletionResponse {
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();

    for block in response.content {
        match block {
            AnthropicResponseContent::Text { text } => {
                content.push(text);
            }
            AnthropicResponseContent::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                });
            }
            AnthropicResponseContent::ToolResult { .. } => {
                // Ignore tool results in response
            }
        }
    }

    CompletionResponse {
        content: if content.is_empty() {
            None
        } else {
            Some(content.join("\n"))
        },
        tool_calls,
        usage: Some(Usage {
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        }),
        finish_reason: response.stop_reason,
    }
}

fn parse_anthropic_error(status: u16, body: &str) -> ProviderError {
    if status == 401 {
        return ProviderError::AuthenticationFailed;
    }
    if status == 429 {
        return ProviderError::RateLimited;
    }
    if status == 400 {
        if let Ok(err) = serde_json::from_str::<AnthropicErrorResponse>(body) {
            if let Some(t) = err.error.type_ {
                if t == "invalid_request_error" {
                    if err.error.message.contains("prompt is too long")
                        || err.error.message.contains("context_length_exceeded")
                        || err.error.message.contains("context window")
                    {
                        return ProviderError::ContextLengthExceeded;
                    }
                    if err.error.message.contains("model")
                        && err.error.message.contains("not found")
                    {
                        return ProviderError::ModelNotFound(err.error.message);
                    }
                }
            }
        }
    }
    ProviderError::RequestFailed(format!("HTTP {}: {}", status, body))
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorResponse {
    error: AnthropicErrorDetail,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    type_: Option<String>,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = AnthropicProvider::new(
            "test-key".to_string(),
            "claude-3-7-sonnet-20250219".to_string(),
        );
        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.model(), "claude-3-7-sonnet-20250219");
        assert!(provider.supports_tools());
    }
}
