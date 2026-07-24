// Provider module: LLM provider abstraction

pub mod anthropic;
pub mod openai;
pub mod ollama;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAIProvider;
pub use ollama::OllamaProvider;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Configuration for a provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
}

/// Request for completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
}

/// A message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
}

/// The role of a message sender
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Content of a message
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    ToolResults(Vec<ToolResult>),
}

impl From<String> for MessageContent {
    fn from(text: String) -> Self {
        MessageContent::Text(text)
    }
}

impl From<&str> for MessageContent {
    fn from(text: &str) -> Self {
        MessageContent::Text(text.to_string())
    }
}

/// Result from a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: Option<bool>,
}

/// Definition of a tool available to the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Response from a completion request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

/// A tool call requested by the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Token usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cache_creation_input_tokens: Option<usize>,
    pub cache_read_input_tokens: Option<usize>,
}

impl Usage {
    pub fn total_tokens(&self) -> usize {
        self.input_tokens + self.output_tokens
    }
}

/// Streaming chunk for responses
#[derive(Debug, Clone)]
pub struct Chunk {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub finish_reason: Option<String>,
}

/// Errors that can occur during provider operations
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("API request failed: {0}")]
    RequestFailed(String),

    #[error("Authentication failed")]
    AuthenticationFailed,

    #[error("Rate limited")]
    RateLimited,

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Model not found: {0}")]
    ModelNotFound(String),

    #[error("Context length exceeded")]
    ContextLengthExceeded,

    #[error("Provider not supported: {0}")]
    NotSupported(String),

    #[error("Other error: {0}")]
    Other(String),
}

/// Result type for provider operations
pub type Result<T> = std::result::Result<T, ProviderError>;

/// Convenience alias for Result type
pub type ProviderResult<T> = Result<T>;

/// Trait for LLM providers
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get the provider name
    fn name(&self) -> &str;

    /// Get the model name
    fn model(&self) -> &str;

    /// Check if this provider supports tool use
    fn supports_tools(&self) -> bool {
        true
    }

    /// Get the maximum tokens for this provider/model
    fn max_tokens(&self) -> usize {
        200_000
    }

    /// Complete a request (non-streaming)
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;

    /// Complete a request with streaming
    async fn stream_complete(
        &self,
        request: CompletionRequest,
    ) -> Result<tokio_stream::wrappers::ReceiverStream<Result<Chunk>>>;
}

/// Provider registry for managing multiple providers
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: Vec<std::sync::Arc<dyn Provider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.providers.iter().map(|p| p.name()).collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: std::sync::Arc<dyn Provider>) {
        self.providers.push(provider);
    }

    pub fn get_by_name(&self, name: &str) -> Option<std::sync::Arc<dyn Provider>> {
        self.providers
            .iter()
            .find(|p| p.name() == name)
            .cloned()
    }

    pub fn get_default(&self) -> Option<std::sync::Arc<dyn Provider>> {
        self.providers.first().cloned()
    }

    pub fn list(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
