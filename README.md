# Aura

A production-ready framework for composing AI agents and multi-agent workflows from declarative TOML configuration, with MCP tool integration, RAG pipelines, and an OpenAI-compatible web API. Built on [Rig.rs](https://github.com/0xPlaygrounds/rig) with reliability and operability enhancements.

Key capabilities:

- Declarative agent composition via TOML with multi-provider LLM support and multi-agent serving
- Dynamic [MCP](https://modelcontextprotocol.io) tool discovery across HTTP, SSE, and STDIO transports
- Automatic schema sanitization for OpenAI function-calling compatibility
- RAG pipeline integration with in-memory and external vector stores
- Multi-agent orchestration with coordinator/worker architecture and DAG-based parallel execution
- Dependency-aware multi-wave execution with quality evaluation and iterative re-planning loops

> **Open Alpha** — Aura is under active development. APIs and configuration
> may change between releases. [Issues and feature requests](https://github.com/mezmo/aura/issues)
> are welcome — we'd love your feedback.

## Table of Contents

- [Aura](#aura)
  - [Table of Contents](#table-of-contents)
  - [Project Structure](#project-structure)
  - [Quick Start](./examples/quickstart/README.md)
  - [Developer Setup](#setup)
  - [Usage](#usage)
    - [CLI](#cli)
    - [Web API Server](#web-api-server)
  - [Configuration](#configuration)
    - [Orchestration](#orchestration)
    - [Ollama](#ollama)
    - [Observability](#observability)
  - [Docker Deployment](#docker-deployment)
  - [Development and Testing](#development-and-testing)
  - [Testing](#testing)
  - [Documentation](#documentation)
  - [Architecture](#architecture)
  - [License](#license)

## Project Structure

```text
aura/
├── crates/
│   ├── aura/                # Core library (agent builder + orchestration)
│   ├── aura-config/         # TOML parser and config loader
│   ├── aura-cli/            # CLI for running and testing agents
│   ├── aura-web-server/     # OpenAI-compatible HTTP/SSE server
│   └── aura-test-utils/     # Shared testing utilities
├── compose/                 # Docker Compose (integration + orchestration overlays)
├── configs/                 # E2E test and orchestration configurations
├── examples/                # Example and reference configurations
├── development/             # LibreChat and OpenWebUI setup
├── docs/                    # Architecture and protocol documentation
└── scripts/                 # CI and utility scripts
```

## Setup

1. Install Rust if needed:
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
2. Clone and configure:
   ```bash
   cd aura
   cp examples/reference.toml config.toml
   ```
3. Set required environment variables:
   ```bash
   export OPENAI_API_KEY="your-api-key"
   ```
4. Build:
   ```bash
   cargo build --release
   ```

Security: keep secrets in environment variables and reference them in TOML using `{{ env.VAR_NAME }}`.

## Usage

### CLI

Run a single query:

```bash
cargo run --bin aura-cli -- --config config.toml --query "Hello!"
```

Interactive mode:

```bash
cargo run --bin aura-cli -- --config config.toml
```

Verbose mode:

```bash
cargo run --bin aura-cli -- --config config.toml --verbose --query "Investigate this issue"
```

### Web API Server

Run the server:

```bash
# Default: reads config.toml
cargo run --bin aura-web-server

# Custom config file
CONFIG_PATH=my-config.toml cargo run --bin aura-web-server

# Config directory (serves multiple agents)
CONFIG_PATH=configs/ cargo run --bin aura-web-server

# Host/port override
HOST=0.0.0.0 PORT=3000 cargo run --bin aura-web-server

# Enable Aura custom SSE events
AURA_CUSTOM_EVENTS=true cargo run --bin aura-web-server
```

Core server options:

| Option                       | Env Variable               | Default       | Description                         |
| ---------------------------- | -------------------------- | ------------- | ----------------------------------- |
| `--config`                   | `CONFIG_PATH`              | `config.toml` | Path to TOML config file or directory |
| `--host`                     | `HOST`                     | `127.0.0.1`   | Bind host                           |
| `--port`                     | `PORT`                     | `8080`        | Bind port                           |
| `--streaming-timeout-secs`   | `STREAMING_TIMEOUT_SECS`   | `900`         | Max SSE request duration            |
| `--first-chunk-timeout-secs` | `FIRST_CHUNK_TIMEOUT_SECS` | `30`          | Max time to first provider chunk    |
| `--streaming-buffer-size`    | `STREAMING_BUFFER_SIZE`    | `400`         | SSE backpressure buffer             |
| `--aura-custom-events`       | `AURA_CUSTOM_EVENTS`       | `false`       | Enable `aura.*` events              |
| `--aura-emit-reasoning`      | `AURA_EMIT_REASONING`      | `false`       | Enable `aura.reasoning`             |
| `--tool-result-mode`         | `TOOL_RESULT_MODE`         | `none`        | Tool result streaming: none, open-web-ui, aura |
| `--tool-result-max-length`   | `TOOL_RESULT_MAX_LENGTH`   | `100`         | Max chars before truncation (aura events) |
| `--shutdown-timeout-secs`    | `SHUTDOWN_TIMEOUT_SECS`    | `30`          | Graceful shutdown window            |

Tool result modes:

- `none`: spec-compliant; tool results appear only in model summary.
- `open-web-ui`: tool results emitted through `tool_calls` for OpenWebUI compatibility.
- `aura`: tool results emitted via `aura.tool_complete` events.

API examples:

```bash
# Health
curl http://localhost:8080/health

# List available models (agents)
curl http://localhost:8080/v1/models

# OpenAI-compatible chat completion
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages": [{"role": "user", "content": "Hello"}]}'

# Select a specific agent by name or alias via the model field
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "my-agent", "messages": [{"role": "user", "content": "Hello"}]}'

# Streaming response
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages": [{"role": "user", "content": "Hello"}], "stream": true}'
```

SSE protocol details, event types, and client handling are documented in [docs/streaming-api-guide.md](docs/streaming-api-guide.md).

For LibreChat/OpenWebUI integration, see [development/README.md](development/README.md).

## Configuration

`CONFIG_PATH` can point to a single TOML file or a directory of `.toml` files. When pointed at a directory, Aura loads every `.toml` file and serves each as a selectable agent. Clients choose an agent via the `model` field in chat completion requests — the same field that tools like LibreChat, OpenWebUI, and CLI clients use to present a model picker.

### Multiple Agents

To serve multiple agents, create a directory with one TOML file per agent:

```
configs/
├── research-assistant.toml
├── devops-agent.toml
└── code-reviewer.toml
```

```bash
CONFIG_PATH=configs/ cargo run --bin aura-web-server
```

Each agent is identified by its `alias` (if set) or `name`. Clients discover available agents via `GET /v1/models` and select one by passing its identifier as the `model` field in requests. When no `model` is specified, the server resolves the agent via `DEFAULT_AGENT`, or automatically when only one config is loaded.

The `alias` field provides a stable, client-facing identifier that is independent of the agent's display name:

```toml
[agent]
name = "DevOps Assistant"
alias = "devops"             # clients send "model": "devops"
system_prompt = "You are a DevOps expert."
model_owner = "mezmo"        # override owned_by in /v1/models (defaults to LLM provider)
```

Aliases must be unique across all loaded configs. If two configs share the same `name` and neither has an alias, loading fails with a validation error.

### Configuration Sections

Configuration sections:

- `[llm]`: provider and model configuration.
- `[agent]`: identity, system prompt, and runtime behavior.
- `[[vector_stores]]`: optional RAG/vector store configuration.
- `[mcp]` and `[mcp.servers.*]`: MCP configuration, schema sanitization, and transports.

Supported providers: OpenAI, Anthropic, Bedrock, Gemini, and Ollama.

Supported MCP transports:

- `http_streamable` (recommended for production)
- `sse`
- `stdio` - for local processes. In production, bridge through [mcp-proxy](https://github.com/sparfenyuk/mcp-proxy) to avoid Rig.rs STDIO lifecycle issues:

```bash
mcp-proxy --port=8081 --host=127.0.0.1 npx your-mcp-server
```

Then point your config at the HTTP/SSE endpoint instead.

`headers_from_request` can forward incoming request headers to MCP servers for per-request auth. See [development/README.md](development/README.md) for practical examples.

`turn_depth` controls how many tool-calling rounds can happen in a single turn. Higher values allow multi-step tool workflows before final response generation. This acts as a failsafe to prevent models from spinning out in unbounded tool-call loops.

`context_window` sets the context window size (in tokens) for the agent, used for usage percentage reporting in `aura.session_info` streaming events.

The complete starter configuration is in [examples/reference.toml](examples/reference.toml). Minimal per-provider configs are in `examples/minimal/` and complete agent examples are in `examples/complete/`.

Minimal example:

```toml
[llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.2"

[mcp.servers.my_server]
transport = "http_streamable"
url = "http://localhost:8081/mcp"
headers = { "Authorization" = "Bearer {{ env.MCP_TOKEN }}" }

[agent]
name = "Assistant"
alias = "my-assistant"       # optional: stable client-facing identifier
system_prompt = "You are a helpful assistant."
turn_depth = 2
```

Validate config parsing quickly:

```bash
cargo run -p aura-config --bin debug_config
```

### Orchestration

Enable orchestration mode in config:

```toml
[orchestration]
enabled = true
quality_threshold = 0.8
max_planning_cycles = 3
tools_in_planning = "summary"

[orchestration.worker.operations]
description = "Operational analysis and diagnostics"
preamble = "You are an operations specialist."
mcp_filter = ["ops_*"]
vector_stores = []

[orchestration.worker.knowledge]
description = "Documentation and procedures"
preamble = "You are a knowledge specialist."
mcp_filter = []
vector_stores = ["docs"]
```

Execution loop:

- `Plan`: coordinator decomposes the request into a task DAG.
- `Execute`: dependency-ready tasks run in parallel waves on worker agents.
- `Synthesize`: coordinator merges worker outputs into a coherent response.
- `Evaluate`: quality is scored against `quality_threshold`; if needed, the system re-plans until `max_planning_cycles` is reached.

Workers run with isolated task context windows and filtered MCP/vector-store access based on each worker block.

For a fuller multi-worker example, see [configs/example-workers.toml](configs/example-workers.toml).

### Ollama

Aura supports Ollama, including fallback tool-call parsing for models that emit tool calls as text. See `fallback_tool_parsing` in existing config examples if you run local models.

### Observability

OpenTelemetry support is enabled by default via the `otel` feature in both `aura` and `aura-web-server`. Configure your OTLP endpoint using standard environment variables (for example `OTEL_EXPORTER_OTLP_ENDPOINT`) to export traces.

Aura emits spans using the [OpenInference](https://github.com/Arize-ai/openinference/tree/main/spec) semantic convention (`llm.*`, `tool.*`, `input.*`, `output.*`). Rig-originated `gen_ai.*` attributes are translated at export time.

## Docker Deployment

Aura includes containerized deployment assets at the repo root:

- `Dockerfile`: multi-stage build for the web server.
- `docker-compose.yml`: local container deployment wiring.

Run with Docker Compose:

```bash
docker compose up --build
```

Orchestration testing overlays are available in `compose/` (for example `compose/orchestration.yml` and `compose/orchestration-test.yml`).

## Development and Testing

Quick commands:

```bash
# Full local quality checks
make ci

# Individual checks
make fmt
make fmt-check
make test
make lint

# Build targets
make build
make build-release
```

Test CI pipeline locally before pushing:

```bash
./scripts/test-ci.sh
```

The script mirrors Jenkins checks: format, workspace tests, and clippy with warnings denied.

## Testing

Web server integration tests live under `crates/aura-web-server/tests/`.

Run integration workflows:

```bash
# Standard integration suites
make test-integration

# Local integration run against locally started test infra
make test-integration-local

# Orchestration-specific integration suites
make test-integration-orchestration

# Local orchestration integration run
make test-integration-orchestration-local
```

Integration test feature flags (`crates/aura-web-server/Cargo.toml`):

- Parent flag: `integration`
- Suite flags: `integration-streaming`, `integration-header-forwarding`, `integration-mcp`, `integration-events`, `integration-cancellation`, `integration-progress`
- Orchestration suite: `integration-orchestration` (separate from parent `integration`)
- Optional suite: `integration-vector` (requires external Qdrant setup)

Detailed test guidance: [crates/aura-web-server/tests/README.md](crates/aura-web-server/tests/README.md).

## Documentation

- [CHANGELOG.md](CHANGELOG.md): release and version history.
- [docs/request-lifecycle.md](docs/request-lifecycle.md): request flow diagram, lifecycle, timeout, cancellation, and shutdown behavior.
- [docs/streaming-api-guide.md](docs/streaming-api-guide.md): SSE protocol guide, event taxonomy, tool result modes, custom `aura.*` events, and client examples.
- [docs/orchestration-flow-diagram.md](docs/orchestration-flow-diagram.md): prompt router, coordinator/worker architecture, and orchestration loop diagrams.
- [docs/toml-schema-design.md](docs/toml-schema-design.md): TOML configuration schema reference.
- [docs/rust-config-structs.md](docs/rust-config-structs.md): Rust config struct mapping.
- [docs/rig-tool-execution-order.md](docs/rig-tool-execution-order.md): tool execution ordering analysis.
- [docs/rig-fork-changes.md](docs/rig-fork-changes.md): Rig fork changes and rationale.
- [development/README.md](development/README.md): LibreChat/OpenWebUI setup and header-forwarding examples.

## Architecture

Aura separates concerns across crates:

- `aura`: runtime agent building, MCP integration, orchestration, and vector workflows.
- `aura-config`: typed TOML parsing and validation.
- `aura-cli`: command-line interface for local/interactive runs.
- `aura-web-server`: OpenAI-compatible REST/SSE serving layer.

This separation means:

- Embeddable core: use `aura` directly in any Rust application without config file dependencies.
- Flexible config: `aura-config` can be extended to support other formats (JSON, YAML).
- Testable boundaries: each crate has focused responsibilities and clear interfaces.

Prompt routing and execution model:

- `build_streaming_agent()` routes requests based on `orchestration.enabled`.
- Direct Mode (`orchestration.enabled = false`): single `Agent` handles the turn.
- Orchestration Mode (`orchestration.enabled = true`): `Orchestrator` coordinates worker execution.
- Both `Agent` and `Orchestrator` implement `StreamingAgent`, so they are interchangeable at the API boundary.

Orchestrator components and loop:

- Coordinator agent: plans task DAGs, synthesizes outputs, and evaluates quality.
- Worker agents: per-task instances with filtered MCP tools and vector stores.
- Persistence/event layers: track plan state, task outcomes, and stream orchestration events.
- Loop: Plan -> Execute (dependency waves) -> Synthesize -> Evaluate -> optional re-plan.

Request execution and cancellation flow are documented in [docs/request-lifecycle.md](docs/request-lifecycle.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
