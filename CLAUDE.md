# CLAUDE.md - Project Documentation

## Overview
Aura is a TOML-based configuration system for composing Rig.rs AI agents with MCP tools and RAG pipelines.

## Current Status: Production Ready

All major features complete:
- Bounded streaming with custom aura events
- Rig 0.28 upgrade with ProviderAgent architecture
- Configurable MCP header forwarding (`headers_from_request` with static TOML fallback)
- Request-scoped MCP progress and cancellation
- Client disconnect detection with MCP `notifications/cancelled`
- Multi-agent orchestration mode with coordinator/worker architecture and DAG execution

**Pending**: Upstream Rig PRs - StreamingPromptHook fix + Content-Type header fix

---

## Quick Start

```bash
# Build
cargo build --release

# Start web server (default config.toml)
cargo run --bin aura-web-server

# Start with orchestration config
CONFIG_PATH=configs/example-math-orchestration.toml AURA_CUSTOM_EVENTS=true cargo run --bin aura-web-server

# Run integration tests (local, requires Docker)
make test-integration-local
```

## Project Structure

```
aura/
├── crates/
│   ├── aura/                 # Core library (agent builder + orchestration)
│   ├── aura-config/          # TOML parsing and configuration
│   ├── aura-web-server/      # OpenAI-compatible API
│   └── aura-test-utils/      # Shared testing utilities
├── compose/                  # Docker Compose (integration + orchestration overlays)
├── configs/                  # E2E test and orchestration configurations
├── deployment/               # Helm charts and K8s manifests
├── development/              # LibreChat and OpenWebUI setup
├── docs/                     # Architecture and protocol documentation
├── examples/                 # Example and reference configurations
├── scripts/                  # CI and utility scripts
└── e2e-eval/                 # E2E eval scripts and results (gitignored)
```

## Key Features

### Configuration System
- TOML-based declarative configuration
- Environment variable resolution (`{{ env.VAR }}`)
- Support for multiple LLM providers (OpenAI, Anthropic, Bedrock, Gemini, Ollama)
- Dynamic tool registration

### MCP Integration
- **HTTP Transport**: Full authentication and tool execution
- **SSE Transport**: AWS Knowledge Base integration
- **STDIO Transport**: Tool discovery
- **Header Forwarding**: `headers_from_request` mappings with static TOML `headers` as fallback
- **Cancellation**: `notifications/cancelled` propagation on client disconnect

### Streaming
- OpenAI-compatible SSE streaming (`/v1/chat/completions`)
- Custom `aura.*` events (opt-in via `AURA_CUSTOM_EVENTS=true`):
  - `aura.session_info`, `aura.tool_requested`, `aura.tool_start`, `aura.tool_complete`, `aura.reasoning`, `aura.progress`, `aura.worker_phase`, `aura.tool_usage`, `aura.usage`
- Request cancellation on timeout or client disconnect
- Two-phase graceful shutdown: new requests rejected immediately (503), in-flight streams get configurable grace period (`SHUTDOWN_TIMEOUT_SECS`, default 30s)

### Orchestration (Multi-Agent)
- Coordinator/worker architecture with DAG-based parallel task execution
- Dependency-aware multi-wave execution with quality evaluation
- Iterative re-planning loops (`quality_threshold`, `max_planning_cycles`)
- Three-way routing: direct answer, orchestrated plan, clarification
- 11 `aura.orchestrator.*` SSE events for real-time visibility (see `docs/streaming-api-guide.md`)

## Environment Setup

```bash
export OPENAI_API_KEY="your-key"
export ANTHROPIC_API_KEY="your-key"  # Optional
export MEZMO_API_KEY="your-key"       # For Mezmo MCP
export AWS_PROFILE="your-profile"     # For Knowledge Base
export AWS_REGION="your-region"       # For Knowledge Base
```

## Architecture

### Dependencies
- **rig-core 0.28**: ProviderAgent architecture (via fork for StreamingPromptHook fix)
- **rmcp 0.12**: MCP client with cancellation support
- **Rig Fork**: `mezmo/rig` branch `mshearer/LOG-23351-openai-reasoning`

### Key Modules
- `provider_agent.rs` - Type-erased streaming across providers
- `stream_events.rs` - Custom aura SSE events
- `request_cancellation.rs` - Request lifecycle management
- `tool_event_broker.rs` - FIFO queue for tool_call_id correlation (see critical assumption below)
- `orchestration/` - Multi-agent coordinator, workers, DAG execution, orchestration SSE events

### Critical Assumption: Rig Sequential Tool Execution

The `tool_event_broker` uses a FIFO queue for correlating `tool_call_id` between hook and MCP execution contexts. **This relies on Rig 0.28 streaming mode executing tools sequentially.**

**If upgrading Rig**, verify this assumption by reviewing:
- `rig-core/src/agent/prompt_request/streaming.rs`
- Look for `.await` between `on_tool_call` and `on_tool_result` (ensures sequential)
- Check for `FuturesUnordered` or parallel execution patterns (would break FIFO)

Confirmed sequential as of Rig 0.28: the streaming handler `.await`s each tool call inline. See `docs/rig-tool-execution-order.md`.

## CI/CD

**Status**: Jenkins/Makefile complete, Helm charts and K8s manifests in `deployment/`

```bash
make build          # Build release binary
make test           # Run all tests
make docker-build   # Build Docker image
make lint           # Run clippy + fmt check
```

## E2E Eval

The `e2e-eval/` directory contains scripts for running E2E orchestration tests against the math-MCP setup.

```bash
# Prerequisites: cargo build --release, llama-server on 11435 (for local models)

# Multi-turn session E2E (dependent prompts, session history injection)
PROMPT_SET=dependent ./e2e-eval/run-session-e2e.sh \
  configs/math-orchestration-opus-bedrock.toml \
  configs/math-orchestration-glm.toml

# Single-turn model comparison (multiple iterations, timing stats)
./e2e-eval/run-model-comparison.sh 3 \
  configs/math-orchestration-glm.toml \
  configs/math-orchestration-qwen3.toml

# Analyze session persistence artifacts
python3 e2e-eval/analyze-session-history-eval.py \
  --memory-dir /tmp/aura-math-opus-bedrock \
  --session-id session_e2e_<ts>_opus-bedrock

# Parse SSE captures from comparison runs
python3 e2e-eval/parse-results.py e2e-eval/results-<timestamp>
```

Scripts auto-start math-mcp and cycle the aura server per config. Model name derived from config filename. Results output to gitignored `session-results-*/` and `results-*/` dirs.

Per-model configs live in `configs/math-orchestration-*.toml`. Local llama-server model aliases use `-p<N>` suffix for parallel slot profiles (e.g., `glm-64k-p6`, `qwen35-64k-p3`).

## Documentation

- `README.md` - User-facing documentation
- `CHANGELOG.md` - Auto-generated version history
- `docs/streaming-api-guide.md` - SSE streaming, custom events, and orchestration events
- `docs/ollama-guide.md` - Ollama configuration, fallback tool parsing, and local model guidance
- `docs/request-lifecycle.md` - Request flow, lifecycle, timeout, cancellation, and shutdown
- `docs/rig-tool-execution-order.md` - Tool execution order analysis
- `docs/rig-fork-changes.md` - Rig fork changes and rationale
