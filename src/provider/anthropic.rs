// Anthropic Claude provider implementation

use super::{
    Chunk, CompletionRequest, CompletionResponse, Message, MessageContent, MessageRole,
    Provider, ProviderError, ProviderResult, ToolCall, ToolDefinition, Usage,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Anthropic Claude provider
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    max_tokens: usize,
    base_url: String,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider
    pub fn new(api_key: String, model: String) -> Self {
        Self::with_config(api_key, model, None)
    }

    /// Create with custom base URL
    pub fn with_config(api_key: String, model: String, base_url: Option<String>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
            // Output (generation) token budget, not the context window size.
            max_tokens: 8192,
            base_url: base_url.unwrap_or_else(|| {
                "https://api.anthropic.com/v1/messages".to_string()
            }),
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

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let anthropic_request = self.build_request(request);

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
        // For now, return a non-streaming response wrapped in a stream
        // TODO: Implement true streaming
        let (tx, rx) = tokio::sync::mpsc::channel(1);

        let response = self.complete(request).await?;

        let chunk = Chunk {
            content: response.content,
            tool_calls: if response.tool_calls.is_empty() {
                None
            } else {
                Some(response.tool_calls)
            },
            finish_reason: response.finish_reason,
        };

        tx.send(Ok(chunk))
            .await
            .map_err(|e| ProviderError::Other(e.to_string()))?;

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
    #[serde(skip_serializing)]
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
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
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
                    content: r.content,
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
    ToolResult { tool_use_id: String, content: String },
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
                    if err.error.message.contains("model") && err.error.message.contains("not found") {
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
        let provider = AnthropicProvider::new("test-key".to_string(), "claude-3-7-sonnet-20250219".to_string());
        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.model(), "claude-3-7-sonnet-20250219");
        assert!(provider.supports_tools());
    }
}
