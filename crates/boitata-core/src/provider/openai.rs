// OpenAI GPT provider implementation
//
// API reference:
// - Chat Completions:  https://platform.openai.com/docs/api-reference/chat/create
// - Function calling:  https://platform.openai.com/docs/guides/function-calling
// - Authentication:    https://platform.openai.com/docs/api-reference/authentication
// - Errors:            https://platform.openai.com/docs/guides/error-codes

use super::{
    Chunk, CompletionRequest, CompletionResponse, Message, MessageContent, MessageRole, Provider,
    ProviderError, ProviderResult, ToolCall, ToolDefinition, Usage, tool_content_text,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

/// OpenAI provider
#[derive(Debug, Clone)]
pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    model: String,
    max_tokens: usize,
    base_url: String,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider
    pub fn new(api_key: String, model: String) -> Self {
        Self::with_config(api_key, model, None)
    }

    /// Create with custom base URL
    pub fn with_config(api_key: String, model: String, base_url: Option<String>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
            max_tokens: 128_000,
            // Chat Completions endpoint.
            // See https://platform.openai.com/docs/api-reference/chat/create
            base_url: base_url
                .unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".to_string()),
        }
    }

    /// Set the maximum tokens
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn build_request(&self, request: CompletionRequest) -> OpenAIRequest {
        let messages = to_openai_messages(request.system, request.messages);

        let tools = request.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t: ToolDefinition| OpenAITool {
                    tool_type: "function".to_string(),
                    function: OpenAIFunction {
                        name: t.name,
                        description: t.description,
                        parameters: t.input_schema,
                    },
                })
                .collect()
        });

        OpenAIRequest {
            model: self.model.clone(),
            messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            tools,
        }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let openai_request = self.build_request(request);

        // POST /v1/chat/completions with bearer auth.
        // See https://platform.openai.com/docs/api-reference/chat/create
        let response = self
            .client
            .post(&self.base_url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&openai_request)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        if !status.is_success() {
            return Err(parse_openai_error(status.as_u16(), &body));
        }

        let openai_response: OpenAIResponse = serde_json::from_str(&body)
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        convert_openai_response(openai_response)
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

/// Convert internal messages into OpenAI's chat message format.
///
/// The system prompt is passed separately in `CompletionRequest`, so it's
/// prepended here as a leading `system` message. Tool results expand into one
/// `tool` message each.
fn to_openai_messages(system: Option<String>, messages: Vec<Message>) -> Vec<OpenAIMessage> {
    let mut out = Vec::new();

    if let Some(system) = system {
        out.push(OpenAIMessage {
            role: "system".to_string(),
            content: Some(system),
            tool_calls: None,
            tool_call_id: None,
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
                out.push(OpenAIMessage {
                    role: role.to_string(),
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            MessageContent::ToolResults(results) => {
                for result in results {
                    out.push(OpenAIMessage {
                        // OpenAI tool messages accept only text, so image content
                        // collapses to a short placeholder (see tool_content_text).
                        role: "tool".to_string(),
                        content: Some(tool_content_text(&result.content)),
                        tool_calls: None,
                        tool_call_id: Some(result.tool_call_id),
                    });
                }
            }
            MessageContent::ToolUse { text, tool_calls } => {
                let calls: Vec<OpenAIToolCall> = tool_calls
                    .into_iter()
                    .map(|c| OpenAIToolCall {
                        id: c.id,
                        call_type: "function".to_string(),
                        function: OpenAIFunctionCall {
                            name: c.name,
                            arguments: c.arguments.to_string(),
                        },
                    })
                    .collect();
                out.push(OpenAIMessage {
                    role: "assistant".to_string(),
                    content: text,
                    tool_calls: if calls.is_empty() { None } else { Some(calls) },
                    tool_call_id: None,
                });
            }
        }
    }

    out
}

fn convert_openai_response(response: OpenAIResponse) -> ProviderResult<CompletionResponse> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::InvalidResponse("no choices in response".to_string()))?;

    // OpenAI returns tool call arguments as a JSON *string*; parse into a value.
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tc| {
            let arguments = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            ToolCall {
                id: tc.id,
                name: tc.function.name,
                arguments,
            }
        })
        .collect();

    Ok(CompletionResponse {
        content: choice.message.content,
        tool_calls,
        usage: response.usage.map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }),
        finish_reason: choice.finish_reason,
    })
}

fn parse_openai_error(status: u16, body: &str) -> ProviderError {
    match status {
        401 => ProviderError::AuthenticationFailed,
        429 => ProviderError::RateLimited,
        404 => ProviderError::ModelNotFound(body.to_string()),
        _ => ProviderError::RequestFailed(format!("HTTP {}: {}", status, body)),
    }
}

// Types for the OpenAI Chat Completions API

#[derive(Debug, Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    // OpenAI deprecated `max_tokens`; current models (including the o-series
    // reasoning models, which reject `max_tokens` outright) expect
    // `max_completion_tokens`. Most OpenAI-compatible proxies accept it too.
    #[serde(
        rename = "max_completion_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
}

#[derive(Debug, Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAIFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAIFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIFunctionCall {
    name: String,
    /// JSON-encoded arguments (a string, per the OpenAI wire format).
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponseMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAIToolCall>,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    prompt_tokens: usize,
    completion_tokens: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = OpenAIProvider::new("test-key".to_string(), "gpt-4o".to_string());
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model(), "gpt-4o");
        assert!(provider.supports_tools());
    }

    #[test]
    fn test_system_prompt_becomes_leading_message() {
        let messages = to_openai_messages(
            Some("You are helpful".to_string()),
            vec![Message {
                role: MessageRole::User,
                content: MessageContent::Text("hi".to_string()),
            }],
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
    }

    #[test]
    fn test_tool_results_expand_to_tool_messages() {
        use super::super::{ToolContent, ToolResult};
        let messages = to_openai_messages(
            None,
            vec![Message {
                role: MessageRole::Tool,
                content: MessageContent::ToolResults(vec![ToolResult {
                    tool_call_id: "call-1".to_string(),
                    content: vec![ToolContent::text("result")],
                    is_error: Some(false),
                }]),
            }],
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "tool");
        assert_eq!(messages[0].content.as_deref(), Some("result"));
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("call-1"));
    }
}
