# Aura

A production-ready framework for composing AI agents from declarative TOML configuration, with MCP tool integration, RAG pipelines, and an OpenAI-compatible web API. Built on [Rig.rs](https://github.com/0xPlaygrounds/rig) with reliability and operability enhancements.

Key capabilities:

- Declarative agent composition via TOML with multi-provider LLM support
- Dynamic [MCP](https://modelcontextprotocol.io) tool discovery across HTTP, SSE, and STDIO transports
- Automatic schema sanitization for OpenAI function-calling compatibility
- RAG pipeline integration with in-memory and external vector stores
- Embeddable Rust core independent from configuration layer

## Table of Contents

- [Aura](#aura)
  - [Table of Contents](#table-of-contents)
  - [Project Structure](#project-structure)
  - [Setup](#setup)
  - [Usage](#usage)
    - [Web API Server](#web-api-server)
  - [Configuration](#configuration)
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
│   ├── aura/                # Core agent builder library
│   ├── aura-config/         # TOML parser and config loader
│   ├── aura-web-server/     # OpenAI-compatible HTTP/SSE server
│   └── aura-test-utils/     # Shared testing utilities
├── compose/                 # Docker Compose files for integration testing
├── configs/                 # Example configuration files
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
   cp configs/config.example.toml config.toml
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

### Web API Server

Run the server:

```bash
# Default: reads config.toml
cargo run --bin aura-web-server

# Custom config path
CONFIG_PATH=my-config.toml cargo run --bin aura-web-server

# Host/port override
HOST=0.0.0.0 PORT=3000 cargo run --bin aura-web-server

# Enable Aura custom SSE events
AURA_CUSTOM_EVENTS=true cargo run --bin aura-web-server
```

Core server options:

| Option                       | Env Variable               | Default       | Description                         |
| ---------------------------- | -------------------------- | ------------- | ----------------------------------- |
| `--config`                   | `CONFIG_PATH`              | `config.toml` | Path to TOML config                 |
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

# OpenAI-compatible chat completion
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages": [{"role": "user", "content": "Hello"}]}'

# Streaming response
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages": [{"role": "user", "content": "Hello"}], "stream": true}'
```

SSE protocol details, event types, custom events, and client handling are documented in [docs/streaming-api-guide.md](docs/streaming-api-guide.md).

For LibreChat/OpenWebUI integration, see [development/README.md](development/README.md).

## Configuration

Configuration sections:

- `[llm]`: provider and model configuration.
- `[agent]`: system prompt and runtime behavior.
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

The complete starter configuration is in [configs/config.example.toml](configs/config.example.toml).

Minimal example:

```toml
[llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.2"

[mcp.servers.my_server]
transport = "http_streamable"
url = "http://localhost:8080/mcp"
headers = { "Authorization" = "Bearer {{ env.MCP_TOKEN }}" }

[agent]
name = "Assistant"
system_prompt = "You are a helpful assistant."
turn_depth = 2
```

Validate config parsing quickly:

```bash
cargo run -p aura-config --bin debug_config
```

### Ollama

Aura supports Ollama, including fallback tool-call parsing for model outputs that emit tool calls as text. Full setup, parameter guidance, and model caveats are in [docs/ollama-guide.md](docs/ollama-guide.md).

### Observability

OpenTelemetry support is enabled by default via the `otel` feature in both `aura` and `aura-web-server`. Configure your OTLP endpoint using standard environment variables (for example `OTEL_EXPORTER_OTLP_ENDPOINT`) to export traces.

Aura emits spans using the [OpenInference](https://github.com/Arize-ai/openinference/tree/main/spec) semantic convention (`llm.*`, `tool.*`, `input.*`, `output.*`) rather than the `gen_ai.*` conventions. Rig-originated `gen_ai.*` attributes are automatically translated to OpenInference equivalents at export time. This makes Aura traces natively compatible with [Phoenix](https://github.com/Arize-ai/phoenix) and other OpenInference-aware observability tools.

## Docker Deployment

Aura includes containerized deployment assets at the repo root:

- `Dockerfile`: multi-stage build for the web server.
- `docker-compose.yml`: local container deployment wiring.

Run with Docker Compose:

```bash
docker compose up --build
```

Default container port mapping is `3030:3030` in `docker-compose.yml`. Ensure your config path and API key environment variables are set for the container runtime.

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

Run web server integration test workflow:

```bash
./crates/aura-web-server/tests/run_tests.sh
```

Integration test feature flags (`crates/aura-web-server/Cargo.toml`):

- Parent flag: `integration`
- Suite flags: `integration-streaming`, `integration-header-forwarding`, `integration-mcp`, `integration-events`, `integration-cancellation`, `integration-progress`
- Optional suite: `integration-vector` (requires external Qdrant setup)

Detailed test guidance: [crates/aura-web-server/README.md#running-integration-tests](crates/aura-web-server/README.md#running-integration-tests).

## Documentation

- [CHANGELOG.md](CHANGELOG.md): release and version history.
- [docs/request-lifecycle.md](docs/request-lifecycle.md): request flow diagram, lifecycle, timeout, cancellation, and shutdown behavior.
- [docs/streaming-api-guide.md](docs/streaming-api-guide.md): SSE protocol guide, event taxonomy, tool result modes, custom `aura.*` events, and client examples.
- [docs/rig-tool-execution-order.md](docs/rig-tool-execution-order.md): tool execution ordering analysis.
- [docs/rig-fork-changes.md](docs/rig-fork-changes.md): Rig fork changes and rationale.
- [development/README.md](development/README.md): LibreChat/OpenWebUI setup and header-forwarding examples.

## Architecture

Aura separates concerns across crates:

- `aura`: runtime agent building, MCP integration, tool orchestration, and vector workflows.
- `aura-config`: typed TOML parsing and validation.
- `aura-web-server`: OpenAI-compatible REST/SSE serving layer.

This separation means:

- **Embeddable core** - use `aura` directly in any Rust application without config file dependencies.
- **Flexible config** - `aura-config` can be extended to support other formats (JSON, YAML).
- **Testable boundaries** - each crate has focused responsibilities and clear interfaces.

Key architectural characteristics:

- Dynamic MCP tool discovery at runtime.
- Automatic schema sanitization (anyOf, missing types, optional parameters) driven by OpenAI function-calling requirements — MCP tool schemas are transformed at discovery time to conform to OpenAI's strict subset of JSON Schema.
- Header forwarding support (`headers_from_request`) for per-request MCP auth delegation.
- Config-driven composition with embeddable Rust core.

Request execution and cancellation flow are documented in [docs/request-lifecycle.md](docs/request-lifecycle.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
