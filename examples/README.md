# Examples

Example TOML configurations for Aura agents.

## Quick Start

Copy the reference config and customize:

```bash
cp examples/reference.toml config.toml
# Edit config.toml with your API key and settings
cargo run --bin aura-web-server
```

Or use a provider quickstart directly:

```bash
export OPENAI_API_KEY="sk-..."
CONFIG_PATH=examples/providers/openai.toml cargo run --bin aura-web-server
```

## Structure

```
examples/
├── reference.toml          # Full annotated config — every option documented
├── providers/              # Minimal quickstart configs (just add an API key)
│   ├── openai.toml
│   ├── anthropic.toml
│   ├── ollama.toml         # No API key needed
│   ├── bedrock.toml
│   └── gemini.toml
└── agents/                 # Real-world agent compositions
    ├── devops-assistant.toml           # GitHub MCP
    ├── incident-response-mezmo.toml    # PagerDuty + Mezmo
    ├── incident-response-datadog.toml  # PagerDuty + Datadog
    └── kubernetes-sre.toml            # K8s MCP + Prometheus
```

### providers/

Minimal configs to get a working agent with a single LLM provider. No MCP tools — just a chat agent. Good starting points for adding your own tools.

### agents/

Complete agent configurations that combine an LLM provider with real MCP tool servers. Each file documents its prerequisites and required environment variables.
