# Providers

Boitata's provider layer is a multi-provider abstraction over the `Provider`
trait. Three providers ship built-in:

| Provider | `provider` value | Models |
|----------|------------------|--------|
| Anthropic | `"anthropic"` | Claude (Sonnet, Opus, Haiku) |
| OpenAI | `"openai"` | GPT (GPT-4o, GPT-4o-mini), and any OpenAI-compatible endpoint |
| Ollama | `"ollama"` | Local models via Ollama |

## z.ai (GLM models)

[z.ai](https://z.ai) exposes both an OpenAI-compatible and an
Anthropic-compatible API, so either provider works against it.

### OpenAI-compatible (recommended)

Matches the `openai` provider exactly:

```toml
provider = "openai"
model    = "glm-4.6"
base_url = "https://api.z.ai/api/paas/v4/chat/completions"
max_tokens = 4096
```

### Anthropic-compatible

Uses the native Anthropic Messages API:

```toml
provider = "anthropic"
model    = "glm-4.6"
base_url = "https://api.z.ai/api/anthropic/v1/messages"
max_tokens = 4096
```

Then:

```bash
export BOITATA_API_KEY="your-z.ai-key"
./target/release/boitata run "Say hello and confirm the connection works"
```

> The `anthropic` provider authenticates with the `x-api-key` header. If z.ai's
> Anthropic endpoint rejects it in favor of `Authorization: Bearer`, prefer the
> OpenAI-compatible config above.

## Local models with Ollama

No API key required — just point at a running Ollama instance:

```toml
provider = "ollama"
model    = "llama3.2"
base_url = "http://localhost:11434"
```

## API key precedence

For any provider, leave `api_key` blank in the file and export it instead — the
`BOITATA_API_KEY` env var takes precedence over the config file:

```bash
export BOITATA_API_KEY="your-key"
```
