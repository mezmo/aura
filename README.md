# AURA

AURA is an agentic harness that turns an LLM model into a reliable, autonomous service capable of executing real SRE work. AURA provides the guardrails, API servers, state management, authentication, streaming, error handling, and tool integrations necessary to run AI SRE agents safely in production.

Key capabilities:

- Declarative agent composition via TOML with multi-provider LLM support and multi-agent serving
- Dynamic [MCP](https://modelcontextprotocol.io) tool discovery via HTTP streamable, SSE, and STDIO transports
- Automatic schema sanitization for OpenAI function-calling compatibility
- Vector search integration with Qdrant and AWS Bedrock Knowledge Base
- Embeddable Rust core independent from configuration layer
- Multi-agent orchestration with coordinator/worker architecture and DAG-based parallel execution
- Dependency-aware multi-wave execution with plan/execute loops
- [A2A protocol](https://github.com/a2a-protocol) support for agent-to-agent interoperability

## Table of Contents

- [Quick Start](#quick-start)
- [Project Structure](#project-structure)
- [Development Setup](#development-setup)
- [Usage](#usage)
  - [Web API Server](#web-api-server)
  - [Client-Side Tools](#client-side-tools)
- [Configuration](#configuration)
  - [Multiple Agents](#multiple-agents)
  - [Configuration Sections](#configuration-sections)
  - [Orchestration](#orchestration)
  - [Scratchpad (Context Window Management)](#scratchpad-context-window-management)
  - [Ollama](#ollama)
  - [Observability](#observability)
- [Development and Testing](#development-and-testing)
- [Testing](#testing)
- [Documentation](#documentation)
- [Architecture](#architecture)

## Quick Start

```bash
cp .env.example .env            # set your LLM provider, model, and API key
docker compose up -d            # starts Aura (orchestrator mode) + LibreChat + Phoenix
docker exec -it aura ./aura-cli # chat with the orchestrator from your terminal
```

Aura boots in **orchestrator mode**: a coordinator routes each request — answering simple ones directly and decomposing complex ones across specialized workers. The bundled `aura-cli` connects to the in-container server automatically and renders the coordinator's plan and worker activity as it streams.

Prefer a browser? Open <http://localhost:3080> to chat in LibreChat, or <http://localhost:6006> to inspect traces in Phoenix.

**[Full quickstart guide](docs/quickstart.md)** — provider setup (OpenAI, Anthropic, Ollama, llama-server), adding MCP tools, enabling vector search, serving multiple agents, and troubleshooting.

### More Quickstarts

- **[Orchestration — Math MCP](examples/quickstart-orchestration-math/README.md)** — Multi-agent orchestration with coordinator/worker architecture
- **[Kubernetes SRE](examples/quickstart-k8s-sre/README.md)** — AI-powered SRE agent on KIND with Kubernetes and Prometheus MCP servers
- **[Example Configs](examples/README.md)** — Minimal per-provider configs and complete agent compositions

## Project Structure

```text
aura/
├── crates/
│   ├── aura/                # Core library (agent builder + orchestration)
│   ├── aura-cli/            # Interactive terminal client (HTTP + standalone modes)
│   ├── aura-config/         # TOML parser and config loader
│   ├── aura-events/         # Shared SSE event types
│   ├── aura-test-utils/     # Shared testing utilities
│   └── aura-web-server/     # OpenAI-compatible HTTP/SSE server
├── compose/                 # Docker Compose (integration + orchestration overlays)
├── configs/                 # E2E test and orchestration configurations
├── deployment/              # Helm charts and K8s manifests
├── docs/                    # Architecture and protocol documentation
├── examples/                # Example and reference configurations
├── scripts/                 # CI and utility scripts
└── tests/                   # Integration test fixtures and helpers
```

## Development Setup

For building AURA from source without Docker.

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
4. Build and run:
   ```bash
   cargo run --bin aura-web-server
   ```

Security: keep secrets in environment variables and reference them in TOML using `{{ env.VAR_NAME }}`.

## Usage

### Web API Server

Run the web server:

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

# Kitchen sink: all options
CONFIG_PATH=configs/ \
  HOST=0.0.0.0 PORT=8080 \
  AURA_CUSTOM_EVENTS=true \
  AURA_EMIT_REASONING=true \
  TOOL_RESULT_MODE=aura \
  OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
  cargo run --bin aura-web-server -- --verbose
```

Core server options:

| Option                       | Env Variable               | Default       | Description                         |
| ---------------------------- | -------------------------- | ------------- | ----------------------------------- |
| `--config`                   | `CONFIG_PATH`              | `config.toml` | Path to TOML config file or directory |
| `--host`                     | `HOST`                     | `127.0.0.1`   | Bind host                           |
| `--port`                     | `PORT`                     | `8080`        | Bind port                           |
| `--server-url`               | `AURA_SERVER_URL`          | host/port     | Canonical public origin published in the A2A agent card (see below) |
| `--streaming-timeout-secs`   | `STREAMING_TIMEOUT_SECS`   | `900`         | Max SSE request duration            |
| `--first-chunk-timeout-secs` | `FIRST_CHUNK_TIMEOUT_SECS` | `30`          | Max time to first provider chunk    |
| `--streaming-buffer-size`    | `STREAMING_BUFFER_SIZE`    | `400`         | SSE backpressure buffer             |
| `--aura-custom-events`       | `AURA_CUSTOM_EVENTS`       | `false`       | Enable `aura.*` events              |
| `--aura-emit-reasoning`      | `AURA_EMIT_REASONING`      | `false`       | Enable `aura.reasoning`             |
| `--tool-result-mode`         | `TOOL_RESULT_MODE`         | `none`        | Tool result streaming: none, open-web-ui, aura |
| `--tool-result-max-length`   | `TOOL_RESULT_MAX_LENGTH`   | `1000`        | Max chars before truncation (aura events) |
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

SSE protocol details, event types, custom events, and client handling are documented in [docs/streaming-api-guide.md](docs/streaming-api-guide.md).

#### A2A Protocol

> **Disabled by default.** A2A endpoints are only activated when the server is started with `--enable-a2a` (or `AURA_ENABLE_A2A=true`). Omitting the flag means no A2A routes are registered and the agent card is not served.

Aura exposes [A2A protocol](https://github.com/a2a-protocol) endpoints for agent-to-agent interoperability. This allows other A2A-compatible agents and clients to discover and interact with Aura agents using a standardized protocol.

```bash
# Agent card (capability discovery)
curl http://localhost:8080/.well-known/agent-card.json

# Send a message via REST
curl -X POST http://localhost:8080/a2a/v1/message:send \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{"message": {"messageId": "msg-001", "role": "ROLE_USER", "parts": [{"text": "Hello"}]}}'

# Send a message via JSON-RPC
curl -X POST http://localhost:8080/a2a/v1/rpc \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "SendMessage", "params": {"message": {"messageId": "msg-002", "role": "ROLE_USER", "parts": [{"text": "Hello"}]}}, "id": 1}'
```

> **Set `AURA_SERVER_URL` when running behind a proxy, load balancer, or in Kubernetes.** The agent card must advertise **absolute** endpoint URLs, and A2A clients use those URLs directly — a relative or wrong-host URL makes `message:send` fail even though the card itself loads. Aura builds the card's URLs from `AURA_SERVER_URL` (or `--server-url`); set it to the externally-reachable origin clients use (e.g. `https://aura.example.com`). When unset, it falls back to the bind host/port, which is only correct for direct local access.

A2A endpoints, transport modes, the agent card URL, task lifecycle, and testing examples are documented in [docs/a2a-implementation.md](docs/a2a-implementation.md).

### Client-Side Tools

> ---
> # **USE AT YOUR OWN RISK**
> ---
>
> **Setting `enable_client_tools = true` on an agent grants the LLM the ability to call tools that execute on the *client's* machine.** When clients (e.g. `aura-cli`) advertise tools like `Shell`, `Read`, or `Update`, the LLM can invoke them and the client will execute them with the privileges of the user running the client. This is functionally equivalent to giving the model a shell prompt on every connecting client.
>
> **The risks are real:**
> - **Prompt injection.** Anything the model reads — a file, an MCP tool output, a vector-store hit, a URL — can contain instructions that hijack the model into running destructive commands. The server cannot tell a legitimate request from an injected one.
> - **Hallucination.** The model can confidently call the wrong tool with the wrong arguments. There is no undo for a `Shell("rm -rf ...")` invocation.
> - **No server-side sandbox.** The server only forwards tool calls; execution happens client-side with full host privileges. Whatever sandboxing exists is the client's responsibility.
> - **Per-agent permission filters reduce blast radius but are not a security boundary.** `client_tool_filter` controls which tools the model *can ask for*, not what they do once invoked.
>
> **Only enable on agents where:**
> - You trust the model, the provider, and every data source the model can read (configs, MCP servers, vector stores, web fetches).
> - You trust every client that will connect with `--enable-client-tools` and the user account it runs under.
> - You and your users accept that worst-case loss (deleted files, leaked credentials, modified source) is acceptable or recoverable.
>
> Disabled by default. Opting an agent in is your decision and your responsibility — and your users'.
>
> See [aura-cli's matching warning](crates/aura-cli/README.md#client-side-tools) for the client-side perspective.

> **Single-agent configurations only.** Client-side tools are not supported in orchestrated (multi-agent) configurations — when `[orchestration].enabled = true`, any `tools` array on the request is dropped with a warning. The reason: the passthrough mechanism requires terminating the user-facing SSE stream with `finish_reason: "tool_calls"`, which doesn't compose with the coordinator/worker pipeline. If you need local tools, use a single-agent config.

The server honors a `tools` array on incoming chat completion requests. Whether those tools are actually attached to the LLM is a **per-agent opt-in** in TOML — there is no server-wide flag. Tools that get attached are registered as **passthrough** tools: the LLM sees them alongside any server-side MCP tools and can call them, but instead of executing server-side, the stream terminates with `finish_reason: "tool_calls"` so the client can run the tool locally and submit the result back as a `role: "tool"` follow-up.

```toml
[agent]
name = "Assistant"
system_prompt = "..."
enable_client_tools = true
client_tool_filter = ["Read", "ListFiles", "Find*"]   # optional; omitted/empty = all
```

`client_tool_filter` is a list of glob patterns matched against the request's `tools[].function.name`. An empty or omitted filter means all client tools are available. A request that supplies tools never reaches an agent that did not opt in.

```bash
# 1) Initial request advertising a client-side tool
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "stream": true,
    "messages": [{"role": "user", "content": "What time is it?"}],
    "tools": [{
      "type": "function",
      "function": {
        "name": "get_current_time",
        "description": "Get the current time",
        "parameters": {"type": "object", "properties": {}}
      }
    }]
  }'

# 2) Stream ends with finish_reason: "tool_calls". The client executes the tool
#    locally and submits the result back in a follow-up request:
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "stream": true,
    "tools": [ ... same tools array ... ],
    "messages": [
      {"role": "user", "content": "What time is it?"},
      {"role": "assistant", "content": null, "tool_calls": [
        {"id": "call_abc", "type": "function",
         "function": {"name": "get_current_time", "arguments": "{}"}}
      ]},
      {"role": "tool", "tool_call_id": "call_abc", "content": "2026-04-30T14:30:00Z"}
    ]
  }'
```

When the loaded agent doesn't opt in (the default), any `tools` field on the request is silently dropped; the server runs MCP tools as usual but never asks the client to execute anything. Per-agent opt-in is the design — accepting client-supplied tool definitions means trusting the client to execute them, so it should be a deliberate config decision. See [aura-cli](crates/aura-cli/README.md#client-side-tools) for the matching client-side flag (`--enable-client-tools`) and how the two halves coordinate.

## Configuration

<details>
<summary>Recent breaking changes</summary>

- **21 April 2026**: `[llm]` moved under `[agent.llm]`; workers may override via `[orchestration.worker.<name>.llm]`. See [migration guide](docs/breaking-changes/20260421-llm-under-agent.md).
- **10 April 2026**: Several fields moved from `[agent]` to `[llm]`; Ollama params consolidated under `[llm.additional_params]`. See [migration guide](docs/breaking-changes/20260410-agent-llm-toml-configuration.md).

</details>

`CONFIG_PATH` can point to a single TOML file or a directory of `.toml` files. When pointed at a directory, AURA loads every `.toml` file and serves each as a selectable agent. Clients choose an agent via the `model` field in chat completion requests — the same field that tools like LibreChat, OpenWebUI, and CLI clients use to present a model picker.

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

- `[agent]`: identity, system prompt, and runtime behavior.
- `[agent.llm]`: provider and model configuration for the agent.
- `[[vector_stores]]`: optional vector search configuration.
- `[mcp]` and `[mcp.servers.*]`: MCP configuration, schema sanitization, and transports.

Supported providers: OpenAI, Anthropic, Bedrock, Gemini, and Ollama.

Supported MCP transports:

- `http_streamable` (recommended for production)
- `sse`
- `stdio` - launches a local child process per request. The [MCP specification](https://modelcontextprotocol.io/specification/2024-11-05/basic/transports) defines this transport for client-side sidecars, not for server deployments. If you need high concurrency, use `http_streamable`.

STDIO configuration uses `cmd` and `args`, both are lists. `cmd[0]` is the executable. Any additional elements in `cmd` are part of the command itself, such as a script path that needs an interpreter. `args` are passed to the spawned process separately:

```toml
# Binary with package arguments
[mcp.servers.my_stdio]
transport = "stdio"
cmd = ["npx"]
args = ["-y", "@modelcontextprotocol/server-everything"]

# Script that needs an interpreter
[mcp.servers.my_script]
transport = "stdio"
cmd = ["python3", "/opt/mcp-servers/weather.py"]
args = ["--verbose"]

# Direct binary
[mcp.servers.my_binary]
transport = "stdio"
cmd = ["/usr/local/bin/mcp-server"]
args = ["--config", "/etc/mcp/config.json"]
```

Tool names are not namespaced by server. If two servers register a tool with the same name, the first one loaded wins silently ([#186](https://github.com/mezmo/aura/issues/186)).

`headers_from_request` can forward incoming request headers to MCP servers for per-request auth.

`turn_depth` controls how many tool-calling rounds can happen in a single turn. Higher values allow multi-step tool workflows before final response generation. This acts as a failsafe to prevent models from spinning out in unbounded tool-call loops.

`context_window` sets the context window size (in tokens) for the agent, used for usage percentage reporting in `aura.session_info` streaming events.

The complete starter configuration is in [examples/reference.toml](examples/reference.toml). Minimal per-provider configs are in `examples/minimal/` and complete agent examples are in `examples/complete/`.

Minimal example:

```toml
[agent]
name = "Assistant"
alias = "my-assistant"       # optional: stable client-facing identifier
system_prompt = "You are a helpful assistant."
turn_depth = 2

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.2"
context_window = 128000

[mcp.servers.my_server]
transport = "http_streamable"
url = "http://localhost:8081/mcp"
headers = { "Authorization" = "Bearer {{ env.MCP_TOKEN }}" }
```

Validate built-in config examples and tests:

```bash
cargo test -p aura-config
```

This runs all config validation tests, including `test_all_shipped_configs_parse` which validates every `.toml` file in `configs/`, `examples/`, and `quickstart.toml`.

To validate your own config file, start the web server or CLI — both validate the config immediately and exit with a clear error if parsing fails, before binding to any port or entering the REPL:

```bash
# Validate via web server (exits on parse error before binding)
cargo run -p aura-web-server -- --config your-config.toml

# Validate via standalone CLI (exits on parse error before REPL)
cargo run -p aura-cli --features standalone-cli -- --standalone --config your-config.toml
```

### Orchestration

Enable orchestration mode in config:

```toml
# Top-level: shared by orchestration persistence and single-agent scratchpad
memory_dir = "/tmp/orchestration-memory"

[orchestration]
enabled = true
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

Each worker inherits `[agent.llm]` by default. To run a worker against a different model (cheaper, faster, bigger context, different provider), add a _complete_ LLM configuration at `[orchestration.worker.<name>.llm]` - this must be a complete LLM configuration not just the individual LLM fields you want to "override":

```toml
[orchestration.worker.formatting.llm]
provider = "anthropic"
api_key = "{{ env.ANTHROPIC_API_KEY }}"
model = "claude-haiku-4-5-20251001"
context_window = 200000
```

The worker's resolved `context_window` is what gets reported in per-worker `aura.session_info` events.

Execution loop:

- `Plan`: coordinator decomposes the request into a task DAG.
- `Execute`: dependency-ready tasks run in parallel waves on worker agents.
- `Continue`: coordinator consolidates worker outputs and routes to a final response, replan, or clarification.

Workers run with isolated task context windows and filtered MCP/vector-store access based on each worker block.

For a fuller multi-worker example, see [configs/example-math-orchestration.toml](configs/example-math-orchestration.toml).

#### Orchestration fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable orchestration mode |
| `max_planning_cycles` | int | `3` | Maximum plan→execute→continue iterations |
| `allow_direct_answers` | bool | `true` | Allow coordinator to answer simple queries directly |
| `allow_clarification` | bool | `true` | Allow coordinator to ask for clarification |
| `tools_in_planning` | string | `"summary"` | Tool visibility for coordinator: `"none"`, `"summary"` (names only), `"full"` (with descriptions) |
| `max_plan_parse_retries` | int | `3` | Retries if coordinator produces unparseable plan JSON |
| `max_tools_per_worker` | int | `10` | Cap on MCP tools exposed to each worker |
| `duplicate_call_nudge_threshold` | int | `3` | Consecutive identical tool calls before appending guidance annotation |
| `duplicate_call_block_threshold` | int | `5` | Consecutive identical tool calls before appending abort annotation and setting escalation flag |
| `worker_system_prompt` | string | — | Optional global system prompt prepended to all workers |
| `coordinator_vector_stores` | list | `[]` | Vector stores available to the coordinator agent |
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
| `llm` | table | inherits `[agent.llm]` | Optional per-worker LLM override — different model (and other `[agent.llm]` fields) while reusing provider credentials |
| `scratchpad` | table | inherits `[agent.scratchpad]` | Optional per-worker scratchpad config override |

### Scratchpad (Context Window Management)

MCP tools can return responses far larger than an LLM's context window — a single Kubernetes workload listing or log export can be tens of thousands of tokens. Without intervention, this fills the context and degrades reasoning quality.

Scratchpad solves this by intercepting large tool outputs and storing them on disk. The LLM gets a summary and eight read-only exploration tools (`head`, `slice`, `grep`, `schema`, `item_schema`, `get_in`, `iterate_over`, `read`) to selectively pull in only the data it needs.

Scratchpad works in both single-agent and orchestration modes. Configure at `[agent.scratchpad]` (applies to the single agent, or provides defaults for orchestration workers) and optionally override per worker at `[orchestration.worker.<name>.scratchpad]`. Set a top-level `memory_dir` for persistence:

```toml
# Top-level — required when scratchpad is enabled. Shared by single-agent
# scratchpad and orchestration persistence.
memory_dir = "/tmp/aura"

[agent.scratchpad]
enabled = true
context_safety_margin = 0.20          # 20% of context reserved for reasoning/output
max_extraction_tokens = 10_000        # cap per extraction tool call
turn_depth_bonus = 6                  # extra ReAct turns when scratchpad is active

[orchestration.worker.data-explorer.scratchpad]
# Override just for this worker
max_extraction_tokens = 5_000
```

**Storage location**:
- Single-agent: `{memory_dir}/scratchpad/`
- Orchestration: `{memory_dir}/{run_id}/iteration-{n}/scratchpad/` (legacy `[orchestration.artifacts].memory_dir` still works as a fallback)

Per-tool interception thresholds are configured at `[mcp.servers.<server>.scratchpad]`. Keys are **glob patterns** (default threshold `5_120` if omitted) that are matched against tool names at interception time:

```toml
[mcp.servers.k8s-sre.scratchpad]
"*_list_*"                  = { min_tokens = 512 }   # broad
"k8s_list_service_monitors" = { min_tokens = 384 }   # specific override
"*"                         = { min_tokens = 4096 }  # catch-all
```

When multiple patterns match the same tool, the **longest (most specific) pattern wins**; on length ties the smallest threshold wins. Token counting uses real BPE tokenization via `tiktoken-rs` — not byte/character heuristics — so `min_tokens` reflects actual model token cost.

**Per-call extraction limit (`max_extraction_tokens`, default 10_000):** every exploration tool checks the size of its result before returning. If a single call would exceed this cap (or the cumulative `ContextBudget`), the tool returns a structured JSON error like `{"error": "head_too_large", "estimated_tokens": ..., "suggestions": [...]}` instead of the content. The LLM sees this as a successful tool result and retries with smaller params — each retry consumes a turn, which is why `turn_depth_bonus` exists.

Each agent (single-agent or orchestration worker) gets a **fresh `ContextBudget`** scoped to that agent's effective LLM's `context_window`. LLM-reported per-turn token counts feed back into the budget as ground truth, so `remaining()` reflects actual context pressure (orchestration via `StreamItem::TurnUsage`, single-agent via the streaming hook). A per-agent `aura.scratchpad_usage` SSE event is emitted when the agent finishes — the same event name fires for both single-agent and worker contexts (it lives in the base `aura.*` namespace, not `aura.orchestrator.*`).

### Ollama

AURA supports Ollama, including fallback tool-call parsing for models that emit tool calls as text. Full setup, parameter guidance, and model caveats are in [docs/ollama-guide.md](docs/ollama-guide.md).

### Observability

OpenTelemetry support is enabled by default via the `otel` feature in both `aura` and `aura-web-server`. Configure your OTLP endpoint using standard environment variables (for example `OTEL_EXPORTER_OTLP_ENDPOINT`) to export traces.

AURA emits spans using the [OpenInference](https://github.com/Arize-ai/openinference/tree/main/spec) semantic convention (`llm.*`, `tool.*`, `input.*`, `output.*`) rather than the `gen_ai.*` conventions. Any `gen_ai.*` attributes from underlying provider libraries (Rig.rs) are automatically translated to OpenInference equivalents at export time. This makes AURA traces natively compatible with [Phoenix](https://github.com/Arize-ai/phoenix) and other OpenInference-aware observability tools.

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
```

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

Detailed test guidance: [crates/aura-web-server/README.md](crates/aura-web-server/README.md).

## Documentation

- [docs/quickstart.md](docs/quickstart.md): getting started guide — setup, customization, architecture, and troubleshooting.
- [CHANGELOG.md](CHANGELOG.md): release and version history.
- [docs/streaming-api-guide.md](docs/streaming-api-guide.md): SSE protocol guide, event taxonomy, tool result modes, custom `aura.*` events, orchestration events, and client examples.
- [docs/request-lifecycle.md](docs/request-lifecycle.md): request flow diagram, lifecycle, timeout, cancellation, and shutdown behavior.
- [docs/ollama-guide.md](docs/ollama-guide.md): Ollama configuration, fallback tool parsing, and local model guidance.
- [docs/rig-fork-changes.md](docs/rig-fork-changes.md): Rig fork changes, tool execution order, and rationale.
- [docs/tracing-spans.md](docs/tracing-spans.md): OpenTelemetry span layout, OpenInference span kinds, and trace parenting for both single-agent and orchestration modes.
- [docs/breaking-changes/20260421-llm-under-agent.md](docs/breaking-changes/20260421-llm-under-agent.md): breaking configuration changes from 21 April 2026 — `[llm]` moved under `[agent.llm]` and per-worker LLM overrides.
- [docs/breaking-changes/20260410-agent-llm-toml-configuration.md](docs/breaking-changes/20260410-agent-llm-toml-configuration.md): breaking configuration changes from 10 April 2026 — field migrations from `[agent]` to `[llm]` and Ollama parameter consolidation.
- [docs/a2a-implementation.md](docs/a2a-implementation.md): A2A protocol endpoints, transport modes (REST and JSON-RPC), task lifecycle, and testing examples.

## Architecture

AURA separates concerns across crates:

- `aura`: runtime agent building, MCP integration, orchestration, and vector workflows.
- `aura-config`: typed TOML parsing and validation.
- `aura-events`: shared SSE event types (`AuraStreamEvent`, `OrchestrationStreamEvent`) — lightweight, no agent dependencies.
- `aura-web-server`: OpenAI-compatible REST/SSE serving layer.
- `aura-cli`: interactive terminal client with HTTP and standalone modes.

This separation means:

- Embeddable core: use `aura` directly in any Rust application without config file dependencies.
- Shared event types: `aura-events` can be consumed by any Rust client without pulling in the full agent stack.
- Testable boundaries: each crate has focused responsibilities and clear interfaces.

Key architectural characteristics:

- Dynamic MCP tool discovery at runtime.
- Automatic schema sanitization (anyOf, missing types, optional parameters) driven by OpenAI function-calling requirements — MCP tool schemas are transformed at discovery time to conform to OpenAI's strict subset of JSON Schema.
- Header forwarding support (`headers_from_request`) for per-request MCP auth delegation.  See [examples/reference.toml](examples/reference.toml) for a practical example.
- Config-driven composition with embeddable Rust core.

Prompt routing and execution model:

- `build_streaming_agent()` routes requests based on `orchestration.enabled`.
- Direct Mode (`orchestration.enabled = false`): single `Agent` handles the turn.
- Orchestration Mode (`orchestration.enabled = true`): `Orchestrator` coordinates worker execution.
- Both `Agent` and `Orchestrator` implement `StreamingAgent`, so they are interchangeable at the API boundary.

Orchestrator components and loop:

- Coordinator agent: plans task DAGs and consolidates worker outputs via continuation.
- Worker agents: per-task instances with filtered MCP tools and vector stores.
- Persistence/event layers: track plan state, task outcomes, and stream orchestration events.
- Loop: Plan -> Execute (dependency waves) -> Continue (respond / plan again / clarify).

Request execution and cancellation flow are documented in [docs/request-lifecycle.md](docs/request-lifecycle.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
