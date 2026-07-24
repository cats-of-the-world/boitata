// Ollama local model provider implementation
//
// API reference:
// - Chat endpoint: https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-chat-completion
// - Tool support:  https://github.com/ollama/ollama/blob/main/docs/api.md#chat-request-with-tools

use super::{
    Chunk, CompletionRequest, CompletionResponse, Message, MessageContent, MessageRole, Provider,
    ProviderError, ProviderResult, ToolCall, ToolDefinition, Usage,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

/// Ollama provider for local models
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: Client,
    base_url: String,
    model: String,
    max_tokens: usize,
}

impl OllamaProvider {
    /// Create a new Ollama provider
    pub fn new(model: String) -> Self {
        Self::with_config(model, None)
    }

    /// Create with custom base URL
    pub fn with_config(model: String, base_url: Option<String>) -> Self {
        Self {
            client: Client::new(),
            model,
            max_tokens: 128_000,
            base_url: base_url.unwrap_or_else(|| "http://localhost:11434".to_string()),
        }
    }

    /// Set the maximum tokens
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn build_request(&self, request: CompletionRequest) -> OllamaRequest {
        let messages = to_ollama_messages(request.system, request.messages);

        let tools = request.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t: ToolDefinition| OllamaTool {
                    tool_type: "function".to_string(),
                    function: OllamaFunction {
                        name: t.name,
                        description: t.description,
                        parameters: t.input_schema,
                    },
                })
                .collect()
        });

        let options = if request.temperature.is_some() || request.max_tokens.is_some() {
            Some(OllamaOptions {
                temperature: request.temperature,
                num_predict: request.max_tokens.map(|n| n as i64),
            })
        } else {
            None
        };

        OllamaRequest {
            model: self.model.clone(),
            messages,
            stream: false,
            tools,
            options,
        }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let ollama_request = self.build_request(request);
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        // POST /api/chat with stream disabled for a single aggregated response.
        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&ollama_request)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        if !status.is_success() {
            return Err(parse_ollama_error(status.as_u16(), &body));
        }

        let ollama_response: OllamaResponse = serde_json::from_str(&body)
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        Ok(convert_ollama_response(ollama_response))
    }

    async fn stream_complete(
        &self,
        request: CompletionRequest,
    ) -> ProviderResult<ReceiverStream<ProviderResult<Chunk>>> {
        // Non-streaming fallback: run `complete` and deliver the result as a
        // single chunk. True SSE streaming can be layered on later.
        let response = self.complete(request).await?;
        Ok(super::single_chunk_stream(response))
    }
}

/// Convert internal messages into Ollama's chat message format.
///
/// Ollama messages carry a plain `content` string plus optional `tool_calls`;
/// tool results are `tool`-role messages. Unlike OpenAI, Ollama tool calls have
/// no IDs, so tool results are matched by role and order.
fn to_ollama_messages(system: Option<String>, messages: Vec<Message>) -> Vec<OllamaMessage> {
    let mut out = Vec::new();

    if let Some(system) = system {
        out.push(OllamaMessage {
            role: "system".to_string(),
            content: system,
            tool_calls: None,
        });
    }

    for message in messages {
        match message.content {
            MessageContent::Text(text) => {
                let role = match message.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                };
                out.push(OllamaMessage {
                    role: role.to_string(),
                    content: text,
                    tool_calls: None,
                });
            }
            MessageContent::ToolResults(results) => {
                for result in results {
                    out.push(OllamaMessage {
                        role: "tool".to_string(),
                        content: result.content,
                        tool_calls: None,
                    });
                }
            }
            MessageContent::ToolUse { text, tool_calls } => {
                let calls: Vec<OllamaToolCall> = tool_calls
                    .into_iter()
                    .map(|c| OllamaToolCall {
                        function: OllamaFunctionCall {
                            name: c.name,
                            arguments: c.arguments,
                        },
                    })
                    .collect();
                out.push(OllamaMessage {
                    role: "assistant".to_string(),
                    content: text.unwrap_or_default(),
                    tool_calls: if calls.is_empty() { None } else { Some(calls) },
                });
            }
        }
    }

    out
}

fn convert_ollama_response(response: OllamaResponse) -> CompletionResponse {
    // Ollama tool calls carry no ID; synthesize one so the agent can pair the
    // resulting tool_use with its tool_result.
    let tool_calls = response
        .message
        .tool_calls
        .into_iter()
        .map(|tc| ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: tc.function.name,
            arguments: tc.function.arguments,
        })
        .collect();

    let content = if response.message.content.is_empty() {
        None
    } else {
        Some(response.message.content)
    };

    CompletionResponse {
        content,
        tool_calls,
        usage: Some(Usage {
            input_tokens: response.prompt_eval_count,
            output_tokens: response.eval_count,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }),
        finish_reason: response.done_reason,
    }
}

fn parse_ollama_error(status: u16, body: &str) -> ProviderError {
    match status {
        404 => ProviderError::ModelNotFound(body.to_string()),
        _ => ProviderError::RequestFailed(format!("HTTP {}: {}", status, body)),
    }
}

// Types for the Ollama chat API

#[derive(Debug, Serialize)]
struct OllamaRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OllamaTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i64>,
}

#[derive(Debug, Serialize)]
struct OllamaTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OllamaFunction,
}

#[derive(Debug, Serialize)]
struct OllamaFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OllamaMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaFunctionCall {
    name: String,
    /// Ollama returns tool arguments as a JSON object (not a string).
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    message: OllamaResponseMessage,
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: usize,
    #[serde(default)]
    eval_count: usize,
}

#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = OllamaProvider::new("llama3.2".to_string());
        assert_eq!(provider.name(), "ollama");
        assert_eq!(provider.model(), "llama3.2");
        assert!(provider.supports_tools());
    }

    #[test]
    fn test_tool_use_message_defaults_empty_content() {
        let messages = to_ollama_messages(
            None,
            vec![Message {
                role: MessageRole::Assistant,
                content: MessageContent::ToolUse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "x".to_string(),
                        name: "search".to_string(),
                        arguments: serde_json::json!({"q": "rust"}),
                    }],
                },
            }],
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "");
        assert!(messages[0].tool_calls.is_some());
    }
}
