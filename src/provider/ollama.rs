// Ollama local model provider implementation

use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, ProviderResult};
use async_trait::async_trait;

/// Ollama provider for local models
#[derive(Debug, Clone)]
pub struct OllamaProvider {
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

    async fn complete(&self, _request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        // TODO: Implement Ollama API call
        Err(ProviderError::Other(
            "Ollama provider not yet implemented".to_string(),
        ))
    }

    async fn stream_complete(
        &self,
        _request: CompletionRequest,
    ) -> ProviderResult<tokio_stream::wrappers::ReceiverStream<ProviderResult<super::Chunk>>> {
        // TODO: Implement Ollama streaming
        Err(ProviderError::Other(
            "Ollama streaming not yet implemented".to_string(),
        ))
    }
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
}
