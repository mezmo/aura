# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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

## Build and Development Commands

```bash
# Build
cargo build --workspace
cargo build --release

# Lint and format
make lint           # cargo clippy --all-targets --all-features -- -D warnings
make fmt            # cargo fmt --all
make fmt-check      # cargo fmt --all -- --check

# Start web server (default config.toml)
cargo run --bin aura-web-server

# Start with orchestration config
CONFIG_PATH=configs/example-math-orchestration.toml AURA_CUSTOM_EVENTS=true cargo run --bin aura-web-server

# Build and run CLI (HTTP mode — connects to aura-web-server)
cargo run -p aura-cli -- --api-url http://localhost:8080

# Build and run CLI (standalone mode — no server needed)
cargo run -p aura-cli --features standalone-cli -- --standalone --config configs/my-agent.toml

# Debug loaded configs
cargo run -p aura-config --bin debug_config -- path/to/configs/
```

## Testing

### Unit Tests

```bash
# All crates
cargo test --workspace

# Single crate
cargo test -p aura
cargo test -p aura-web-server
cargo test -p aura-config
cargo test -p aura-events
cargo test -p aura-cli

# Filter by test name within a crate
cargo test -p aura <test_name_substring>
```

### Integration Tests (requires Docker)

Integration tests **must use `--test-threads=1`** — suites share a server and are order-sensitive.

```bash
# Full suites via Make (handles Docker Compose lifecycle automatically)
make test-integration-local                        # base integration (streaming, MCP, events, cancellation, progress)
make test-integration-orchestration-local          # orchestration (math-mcp)
make test-integration-sre-orchestration-local      # SRE orchestration

# Manual: start infrastructure, run specific suite, tear down
make test-integration-local-up
cargo test --package aura-web-server --features integration-streaming --no-fail-fast -- --test-threads=1
cargo test --package aura-web-server --features integration-mcp --no-fail-fast -- --test-threads=1
cargo test --package aura-web-server --features integration-events --no-fail-fast -- --test-threads=1
cargo test --package aura-web-server --features integration-cancellation --no-fail-fast -- --test-threads=1
cargo test --package aura-web-server --features integration-progress --no-fail-fast -- --test-threads=1
make test-integration-local-down

# Orchestration suites use their own infra
make test-integration-orchestration-local-up
cargo test --package aura-web-server --features integration-orchestration --no-fail-fast -- --test-threads=1
make test-integration-orchestration-local-down
```

Integration test files live in `crates/aura-web-server/tests/`.

## Project Structure

```
aura/
├── crates/
│   ├── aura/                 # Core library (agent builder + orchestration)
│   ├── aura-cli/             # Interactive terminal client (HTTP + standalone modes)
│   ├── aura-config/          # TOML parsing and configuration
│   ├── aura-events/          # Shared SSE event types (lightweight, no agent deps)
│   ├── aura-web-server/      # OpenAI-compatible API
│   └── aura-test-utils/      # Shared testing utilities
├── compose/                  # Docker Compose (integration + orchestration overlays)
├── configs/                  # E2E test and orchestration configurations
├── deployment/               # Helm charts and K8s manifests
├── docs/                     # Architecture and protocol documentation
└── e2e-eval/                 # E2E eval scripts and results (gitignored)
```

## Architecture

### Agent Construction Pipeline

```
AgentConfig (TOML)
    ↓
AgentBuilder (aura-config resolves env vars, loads MCP tool lists)
    ↓ (adds tools: filesystem, RAG, MCP)
Agent (wraps ProviderAgent)
    ↓ implements StreamingAgent
Used by: aura-web-server or aura-cli standalone
```

When `orchestration.enabled = true`, an `Orchestrator` is built instead of `Agent` — both implement `StreamingAgent` so the web server treats them identically.

### Streaming Data Flow

The web server emits two interleaved SSE streams over a single HTTP response:
- **OpenAI-compatible chunks** — `data: {"choices":[...]}` for token-by-token text
- **`aura.*` events** — opt-in via `AURA_CUSTOM_EVENTS=true`

Internally, `StreamItem` is the semantic unit (not SSE bytes). `crates/aura-web-server/src/streaming/handlers.rs` converts `StreamItem` variants into SSE chunks.

### Key Modules (crates/aura/src/)

- `builder.rs` — Agent construction, provider routing
- `mcp.rs` — `McpManager`: tool discovery and execution across HTTP/SSE/STDIO transports
- `provider_agent.rs` — Type-erased streaming across OpenAI/Anthropic/Bedrock/Ollama
- `stream_events.rs` — `AuraStreamEvent` enum and SSE formatting
- `streaming_request_hook.rs` — Rig hook capturing usage tokens
- `tool_event_broker.rs` — FIFO queue correlating `tool_call_id` between hook and MCP contexts
- `request_cancellation.rs` — Client disconnect → MCP `notifications/cancelled`
- `orchestration/orchestrator.rs` — Multi-agent coordinator (252KB, central to orchestration)
- `openinference_exporter.rs` — OTEL Phoenix span generation

### Critical Assumption: Rig Sequential Tool Execution

`tool_event_broker` uses a FIFO queue for `tool_call_id` correlation. **This relies on Rig 0.28 executing tools sequentially** (not in parallel). If upgrading Rig, verify by checking `rig-core/src/agent/prompt_request/streaming.rs` for `.await` between `on_tool_call` and `on_tool_result`. See `docs/rig-tool-execution-order.md`.

### Orchestration (Multi-Agent)

- **Coordinator** decomposes the query into a plan via routing tools (CreatePlan, RespondDirectly, RequestClarification)
- **Workers** execute tasks in dependency-ordered waves; results feed back to coordinator
- **Re-planning**: iterative loop controlled by `quality_threshold` + `max_planning_cycles`
- **Events**: 11 `aura.orchestrator.*` SSE events; see `docs/streaming-api-guide.md`

### aura-events Crate

Lightweight shared types (`AuraStreamEvent`, `OrchestrationStreamEvent`) with only `serde` dependencies — no agent/MCP/provider deps. Allows `aura-cli` to consume events without pulling in the full agent stack. Enable `rmcp-types` feature for zero-copy rmcp interop (used by the `aura` crate internally).

### aura-cli Notes

- Not in workspace `default-members` — must build explicitly: `cargo build -p aura-cli`
- Standalone mode requires `--features standalone-cli` at build time AND `--standalone` at runtime
- `--model` and `--system-prompt` work in both HTTP and standalone modes (with different semantics)

## Environment Variables

```bash
# LLM providers
export OPENAI_API_KEY="your-key"
export ANTHROPIC_API_KEY="your-key"
export MEZMO_API_KEY="your-key"       # For Mezmo MCP
export AWS_PROFILE="your-profile"     # For Knowledge Base
export AWS_REGION="your-region"

# Server behavior
export AURA_CUSTOM_EVENTS=true        # Emit aura.* SSE events alongside OpenAI chunks
export AURA_EMIT_REASONING=true       # Emit aura.reasoning events (Anthropic extended thinking)
export TOOL_RESULT_MODE=aura          # Tool result format: none | open-web-ui | aura
export SHUTDOWN_TIMEOUT_SECS=30       # Grace period for in-flight streams on shutdown

# CLI
export AURA_API_URL=http://localhost:8080
export AURA_API_KEY="your-key"
export AURA_MODEL="gpt-4o"
export AURA_EXTRA_HEADERS="X-Custom: value"
```

## Dependencies

- **rig-core 0.28**: ProviderAgent architecture (Mezmo fork: `mezmo/rig` branch `mshearer/LOG-23351-openai-reasoning`)
- **rmcp 0.12**: MCP client with cancellation support
- **Rust edition 2024**, workspace resolver 3

## E2E Eval

```bash
# Prerequisites: cargo build --release, llama-server on 11435 (for local models)

# Multi-turn session E2E
PROMPT_SET=dependent ./e2e-eval/run-session-e2e.sh \
  configs/math-orchestration-opus-bedrock.toml \
  configs/math-orchestration-glm.toml

# Single-turn model comparison
./e2e-eval/run-model-comparison.sh 3 \
  configs/math-orchestration-glm.toml \
  configs/math-orchestration-qwen3.toml
```

## Documentation

- `docs/streaming-api-guide.md` — SSE streaming, custom events, orchestration events
- `docs/ollama-guide.md` — Ollama config, fallback tool parsing, local model guidance
- `docs/request-lifecycle.md` — Request flow, timeout, cancellation, shutdown
- `docs/rig-tool-execution-order.md` — Tool execution order analysis
- `docs/rig-fork-changes.md` — Rig fork changes and rationale
- `crates/aura-cli/README.md` — CLI usage, backends, features, build instructions
