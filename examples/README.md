# Examples

Example TOML configurations for Aura agents. Each file is a standalone config — pick one and go.

## Quick Start

Copy the reference config and customize:

```bash
cp examples/reference.toml config.toml
# Edit config.toml with your API key and settings
cargo run --bin aura-web-server
```

Or use a minimal config directly:

```bash
export OPENAI_API_KEY="sk-..."
CONFIG_PATH=examples/minimal/openai.toml cargo run --bin aura-web-server
```

If encountering issues and more verbose debugging output is necessary, add the --verbose flag for the running binary
```bash
export OPENAI_API_KEY="sk-..."
CONFIG_PATH=examples/minimal/openai.toml cargo run --bin aura-web-server -- --verbose
```

### Serving Multiple Agents

Point `CONFIG_PATH` at a directory to serve every `.toml` file as a selectable agent:

```bash
CONFIG_PATH=examples/complete/ cargo run --bin aura-web-server
```

Clients discover agents via `GET /v1/models` and select one with the `model` field in chat requests. Each agent is identified by its `alias` (if set) or `name`.

## Structure

```
examples/
├── reference.toml          # Full annotated config — every option documented
├── minimal/                # Bare minimum to get running (just add an API key)
│   ├── openai.toml
│   ├── anthropic.toml
│   ├── ollama.toml         # No API key needed
│   ├── bedrock.toml
│   └── gemini.toml
└── complete/               # Full agent compositions (LLM + MCP tools + prompts)
    ├── devops-assistant.toml           # GitHub MCP
    ├── incident-response-mezmo.toml    # PagerDuty + Mezmo
    ├── incident-response-datadog.toml  # PagerDuty + Datadog
    └── kubernetes-sre.toml            # K8s MCP + Prometheus
```

### minimal/

A working chat agent with a single LLM provider. No MCP tools — just the bare minimum. Good starting points for adding your own tools.

### complete/

Full agent configurations that combine an LLM provider with real MCP tool servers and tailored system prompts. Each file documents its prerequisites and required environment variables.
