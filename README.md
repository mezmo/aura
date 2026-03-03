# Aura

A production-ready framework for composing AI agents and multi-agent workflows from declarative TOML configuration, with MCP tool integration, RAG pipelines, and an OpenAI-compatible web API. Built on [Rig.rs](https://github.com/0xPlaygrounds/rig) with reliability and operability enhancements.

Key capabilities:

- Declarative agent composition via TOML with multi-provider LLM support
- Dynamic [MCP](https://modelcontextprotocol.io) tool discovery across HTTP, SSE, and STDIO transports
- Automatic schema sanitization for OpenAI function-calling compatibility
- RAG pipeline integration with in-memory and external vector stores
- Embeddable Rust core independent from configuration layer
- Multi-agent orchestration with coordinator/worker architecture and DAG-based parallel execution
- Dependency-aware multi-wave execution with quality evaluation and iterative re-planning loops

> **Open Alpha** — Aura is under active development. APIs and configuration
> may change between releases. [Issues and feature requests](https://github.com/mezmo/aura/issues)
> are welcome — we'd love your feedback.

## Table of Contents

- [Aura](#aura)
  - [Table of Contents](#table-of-contents)
  - [Project Structure](#project-structure)
  - [Setup](#setup)
  - [Usage](#usage)
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
│   ├── aura-web-server/     # OpenAI-compatible HTTP/SSE server
│   └── aura-test-utils/     # Shared testing utilities
├── compose/                 # Docker Compose (integration + orchestration overlays)
├── configs/                 # E2E test and orchestration configurations
├── deployment/              # Helm charts and K8s manifests
├── development/             # LibreChat and OpenWebUI setup
├── docs/                    # Architecture and protocol documentation
├── examples/                # Example and reference configurations
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

| Option | Env Variable | Default | Description |
|--------|--------------|---------|-------------|
| `--config` | `CONFIG_PATH` | `config.toml` | Path to TOML configuration file |
| `--host` | `HOST` | `127.0.0.1` | Host to bind to |
| `--port` | `PORT` | `8080` | Port to bind to |
| `--streaming-timeout-secs` | `STREAMING_TIMEOUT_SECS` | `900` | SSE request timeout (15 min default) |
| `--first-chunk-timeout-secs` | `FIRST_CHUNK_TIMEOUT_SECS` | `30` | Max wait for first provider response (0 = disabled) |
| `--streaming-buffer-size` | `STREAMING_BUFFER_SIZE` | `400` | SSE buffer size before backpressure |
| `--aura-custom-events` | `AURA_CUSTOM_EVENTS` | `false` | Enable `aura.*` SSE events |
| `--aura-emit-reasoning` | `AURA_EMIT_REASONING` | `false` | Enable `aura.reasoning` events |
| `--tool-result-mode` | `TOOL_RESULT_MODE` | `none` | Tool result streaming mode |
| `--tool-result-max-length` | `TOOL_RESULT_MAX_LENGTH` | `100` | Max chars for tool results |
| `--shutdown-timeout-secs` | `SHUTDOWN_TIMEOUT_SECS` | `30` | Grace period for in-flight requests on shutdown |

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
allow_direct_answers = true
allow_clarification = true

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

#### Orchestration fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable orchestration mode |
| `quality_threshold` | float | `0.8` | Minimum quality score before accepting a synthesis |
| `max_planning_cycles` | int | `3` | Maximum plan→execute→evaluate iterations |
| `allow_direct_answers` | bool | `true` | Allow coordinator to answer simple queries directly |
| `allow_clarification` | bool | `true` | Allow coordinator to ask for clarification |
| `tools_in_planning` | string | `"summary"` | Tool visibility for coordinator: `"none"`, `"summary"` (names only), `"full"` (with descriptions) |
| `max_phases` | int | `5` | Maximum dependency waves per execution cycle |
| `max_plan_parse_retries` | int | `3` | Retries if coordinator produces unparseable plan JSON |
| `max_tools_per_worker` | int | `10` | Cap on MCP tools exposed to each worker |
| `worker_system_prompt` | string | — | Optional global system prompt prepended to all workers |
| `coordinator_vector_stores` | list | `[]` | Vector stores available to the coordinator agent |
| `memory_dir` | string | — | Directory for cross-iteration artifact persistence |
| `result_artifact_threshold` | int | `4000` | Character count above which worker results are saved as artifacts |
| `result_summary_length` | int | `2000` | Max characters for artifact summaries passed to coordinator |
| `timeouts.per_call_timeout_secs` | int | `0` | Per-tool-call timeout in seconds (0 = disabled) |

#### Worker fields (`[orchestration.worker.<name>]`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `description` | string | *required* | Short description shown to coordinator during planning |
| `preamble` | string | *required* | System prompt for this worker |
| `mcp_filter` | list | `[]` | Glob patterns selecting which MCP tools this worker can use |
| `vector_stores` | list | `[]` | Named vector stores this worker has access to |
| `turn_depth` | int | — | Per-worker tool-call depth limit (overrides `[agent].turn_depth`) |

### Ollama

Aura supports Ollama, including fallback tool-call parsing for models that emit tool calls as text. Full setup, parameter guidance, and model caveats are in [docs/ollama-guide.md](docs/ollama-guide.md).

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

# SRE orchestration integration suites
make test-integration-sre-orchestration

# Local SRE orchestration integration run
make test-integration-sre-orchestration-local
```

Integration test feature flags (`crates/aura-web-server/Cargo.toml`):

- Parent flag: `integration`
- Suite flags: `integration-streaming`, `integration-header-forwarding`, `integration-mcp`, `integration-events`, `integration-cancellation`, `integration-progress`
- Orchestration suite: `integration-orchestration` (separate from parent `integration`)
- SRE orchestration suite: `integration-orchestration-sre` (requires k8s-sre-mcp server config)
- Optional suite: `integration-vector` (requires external Qdrant setup)

Detailed test guidance: [crates/aura-web-server/tests/README.md](crates/aura-web-server/tests/README.md).


## Documentation

- [CHANGELOG.md](CHANGELOG.md): release and version history.
- [docs/streaming-api-guide.md](docs/streaming-api-guide.md): SSE protocol guide, event taxonomy, tool result modes, custom `aura.*` events, orchestration events, and client examples.
- [docs/request-lifecycle.md](docs/request-lifecycle.md): request flow diagram, lifecycle, timeout, cancellation, and shutdown behavior.
- [docs/rig-tool-execution-order.md](docs/rig-tool-execution-order.md): tool execution ordering analysis.
- [docs/ollama-guide.md](docs/ollama-guide.md): Ollama configuration, fallback tool parsing, and local model guidance.
- [docs/rig-fork-changes.md](docs/rig-fork-changes.md): Rig fork changes and rationale.
- [development/README.md](development/README.md): LibreChat/OpenWebUI setup and header-forwarding examples.

## Architecture

Aura separates concerns across crates:

- `aura`: runtime agent building, MCP integration, orchestration, and vector workflows.
- `aura-config`: typed TOML parsing and validation.
- `aura-web-server`: OpenAI-compatible REST/SSE serving layer.

This separation means:

- Embeddable core: use `aura` directly in any Rust application without config file dependencies.
- Flexible config: `aura-config` can be extended to support other formats (JSON, YAML).
- Testable boundaries: each crate has focused responsibilities and clear interfaces.

Key architectural characteristics:

- Dynamic MCP tool discovery at runtime.
- Automatic schema sanitization (anyOf, missing types, optional parameters) driven by OpenAI function-calling requirements — MCP tool schemas are transformed at discovery time to conform to OpenAI's strict subset of JSON Schema.
- Header forwarding support (`headers_from_request`) for per-request MCP auth delegation.
- Config-driven composition with embeddable Rust core.

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
