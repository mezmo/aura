# Examples

Example TOML configurations and advanced quickstarts for AURA agents.

> **New to AURA?** Start with the [Quick Start](../README.md#quick-start) in the
> repo root — it gets a full stack running in under a minute. Come back here when
> you're ready to customize or try orchestration mode.

## Reference Configuration

[`reference.toml`](reference.toml) is the fully annotated config with every option documented. Use it as a starting point:

```bash
cp examples/reference.toml config.toml
# Edit config.toml with your API key and settings
cargo run --bin aura-web-server
```

If encountering issues and more verbose debugging output is necessary, add the --verbose flag for the running binary
```bash
export OPENAI_API_KEY="sk-..."
CONFIG_PATH=examples/minimal/openai.toml cargo run --bin aura-web-server -- --verbose
```

## Minimal Configs

Bare-minimum configs to get running with a single LLM provider. No MCP tools — just add an API key.

```bash
export OPENAI_API_KEY="sk-..."
CONFIG_PATH=examples/minimal/openai.toml cargo run --bin aura-web-server
```

| File | Provider | API key needed? |
|------|----------|-----------------|
| [`openai.toml`](minimal/openai.toml) | OpenAI | Yes |
| [`anthropic.toml`](minimal/anthropic.toml) | Anthropic | Yes |
| [`bedrock.toml`](minimal/bedrock.toml) | AWS Bedrock | AWS credentials |
| [`gemini.toml`](minimal/gemini.toml) | Google Gemini | Yes |
| [`ollama.toml`](minimal/ollama.toml) | Ollama (local) | No |
| [`bootstrap.toml`](minimal/bootstrap.toml) | OpenAI + the token-gated `aura-bootstrap` config agent | Yes |

## Complete Agent Configs

Full agent compositions that combine an LLM provider with real MCP tool servers and tailored system prompts. Each file documents its prerequisites and required environment variables.

| File | Description |
|------|-------------|
| [`devops-assistant.toml`](complete/devops-assistant.toml) | GitHub MCP |
| [`incident-response-mezmo.toml`](complete/incident-response-mezmo.toml) | PagerDuty + Mezmo |
| [`incident-response-datadog.toml`](complete/incident-response-datadog.toml) | PagerDuty + Datadog |
| [`kubernetes-sre.toml`](complete/kubernetes-sre.toml) | K8s MCP + Prometheus |

### Serving Multiple Agents

Point `CONFIG_PATH` at a directory to serve every `.toml` file as a selectable agent:

```bash
CONFIG_PATH=examples/complete/ cargo run --bin aura-web-server
```

Clients discover agents via `GET /v1/models` and select one with the `model` field in chat requests. Each agent is identified by its `alias` (if set) or `name`.

## Advanced Quickstarts

Self-contained Docker Compose setups for specific use cases. Each has its own README with step-by-step instructions.

| Quickstart | Description |
|-----------|-------------|
| [Orchestration — Math MCP](quickstart-orchestration-math/README.md) | Multi-agent orchestration with coordinator/worker architecture and a math tool server |
| [Kubernetes SRE](quickstart-k8s-sre/README.md) | AI-powered SRE agent on KIND with Kubernetes and Prometheus MCP servers |
