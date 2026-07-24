// Configuration loading for Boitata.
//
// Configuration lives in a TOML file (default: `boitata.toml` in the current
// directory) so credentials never need to be passed on the command line. The
// file is git-ignored; commit `boitata.example.toml` as a template instead.

use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Environment variable holding an alternate config file path.
const CONFIG_PATH_ENV: &str = "BOITATA_CONFIG";
/// Environment variable overriding the API key (takes precedence over the file).
const API_KEY_ENV: &str = "BOITATA_API_KEY";
/// Default config file name, looked up in the current directory.
const DEFAULT_CONFIG_FILE: &str = "boitata.toml";

/// Top-level Boitata configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Provider to use: `anthropic`, `openai`, or `ollama`.
    pub provider: String,
    /// Model identifier passed to the provider (e.g. `glm-4.6`).
    pub model: String,
    /// API key. May be omitted here and supplied via `BOITATA_API_KEY` instead.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Override the provider's default endpoint (e.g. an OpenAI-compatible proxy).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Output token budget per request. Defaults to a conservative value.
    #[serde(default)]
    pub max_tokens: Option<usize>,
    /// Maximum agent iterations before giving up.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// Optional custom system prompt for the agent.
    #[serde(default)]
    pub system_prompt: Option<String>,
}

impl Config {
    /// Resolve the config file path from an explicit CLI argument, then the
    /// `BOITATA_CONFIG` env var, then the default `boitata.toml`.
    pub fn resolve_path(explicit: Option<String>) -> PathBuf {
        explicit
            .or_else(|| std::env::var(CONFIG_PATH_ENV).ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE))
    }

    /// Load and parse the config file at `path`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read config file `{}` (copy boitata.example.toml to get started)",
                path.display()
            )
        })?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse config file `{}`", path.display()))
    }

    /// The API key to use: `BOITATA_API_KEY` if set, otherwise the file value.
    pub fn resolve_api_key(&self) -> Option<String> {
        std::env::var(API_KEY_ENV)
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(|| self.api_key.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let config: Config = toml::from_str(
            r#"
            provider = "openai"
            model = "glm-4.6"
            base_url = "https://api.z.ai/api/paas/v4/chat/completions"
            "#,
        )
        .unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "glm-4.6");
        assert_eq!(config.api_key, None);
        assert_eq!(config.max_tokens, None);
    }

    #[test]
    fn test_resolve_path_default() {
        // No explicit arg and (assuming) no env var → default file name.
        unsafe {
            std::env::remove_var(CONFIG_PATH_ENV);
        }
        assert_eq!(
            Config::resolve_path(None),
            PathBuf::from(DEFAULT_CONFIG_FILE)
        );
        assert_eq!(
            Config::resolve_path(Some("custom.toml".to_string())),
            PathBuf::from("custom.toml")
        );
    }
}
