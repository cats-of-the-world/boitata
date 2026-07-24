// OpenAI GPT provider implementation

use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, ProviderResult};
use async_trait::async_trait;

/// OpenAI provider
#[derive(Debug, Clone)]
pub struct OpenAIProvider {
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
            api_key,
            model,
            max_tokens: 128_000,
            base_url: base_url.unwrap_or_else(|| {
                "https://api.openai.com/v1/chat/completions".to_string()
            }),
        }
    }

    /// Set the maximum tokens
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
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

    async fn complete(&self, _request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        // TODO: Implement OpenAI API call
        Err(ProviderError::Other("OpenAI provider not yet implemented".to_string()))
    }

    async fn stream_complete(
        &self,
        _request: CompletionRequest,
    ) -> ProviderResult<tokio_stream::wrappers::ReceiverStream<ProviderResult<super::Chunk>>> {
        // TODO: Implement OpenAI streaming
        Err(ProviderError::Other("OpenAI streaming not yet implemented".to_string()))
    }
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
}
