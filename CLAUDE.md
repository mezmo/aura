# CLAUDE.md - Project Documentation

> If `CLAUDE.local.md` exists in this directory, read it first — it contains current session state.

## Overview
AURA is a TOML-based configuration system for composing Rig.rs AI agents with MCP tools and RAG pipelines.

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

# Build and run CLI (HTTP mode — connects to aura-web-server)
cargo run -p aura-cli -- --api-url http://localhost:8080

# Build and run CLI (standalone mode — no server needed)
cargo run -p aura-cli --features standalone-cli -- --standalone --config configs/my-agent.toml

# Run integration tests (local, requires Docker)
make test-integration-local                        # base integration
make test-integration-orchestration-local          # orchestration integration
make test-integration-sre-orchestration-local      # SRE orchestration integration
```

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
├── configs/                  # Integration test and example configurations
├── deployment/               # Helm charts and K8s manifests
├── docs/                     # Architecture and protocol documentation
├── examples/                 # Example and reference configurations
└── .makefiles/               # Modular Make targets (rust, docker, node, aura)
```

## Key Features

### Configuration System
- TOML-based declarative configuration
- Environment variable resolution (`{{ env.VAR }}`)
- Support for multiple LLM providers (OpenAI, Anthropic, Bedrock, Gemini, Ollama, OpenRouter)
- Dynamic tool registration

### MCP Integration
- **HTTP Transport**: Full authentication and tool execution
- **SSE Transport**: AWS Knowledge Base integration
- **STDIO Transport**: Tool discovery
- **Header Forwarding**: `headers_from_request` mappings with static TOML `headers` as fallback
- **Cancellation**: `notifications/cancelled` propagation on client disconnect
- **Status reporting**: per-server connection state (`Connected`/`Failed(reason)`/`NotAttempted`) is tracked in `McpManager::server_info` and projected to clients via the `aura.mcp_status` SSE event. Transport/auth failures bubble as errors.

### Streaming
- OpenAI-compatible SSE streaming (`/v1/chat/completions`)
- Custom `aura.*` events (opt-in via `AURA_CUSTOM_EVENTS=true`):
  - `aura.session_info`, `aura.mcp_status`, `aura.tool_requested`, `aura.tool_start`, `aura.tool_complete`, `aura.reasoning`, `aura.progress`, `aura.worker_phase`, `aura.tool_usage`, `aura.usage`, `aura.scratchpad_usage`
- Request cancellation on timeout or client disconnect
- Two-phase graceful shutdown: new requests rejected immediately (503), in-flight streams get configurable grace period (`SHUTDOWN_TIMEOUT_SECS`, default 30s)

### Scratchpad (Context Window Management)
- Intercepts large MCP tool outputs and saves them to disk instead of filling the context window
- Eight read-only exploration tools: `head`, `slice`, `grep`, `schema`, `item_schema`, `get_in`, `iterate_over`, `read`
- Per-tool token thresholds configured via `[mcp.servers.<name>.scratchpad]` TOML sections (`min_tokens`, default `5_120`). Keys are **glob patterns** matched against tool names at interception time; when multiple patterns match the same tool, the longest (most specific) wins, ties broken by smallest threshold
- Token counting uses **tiktoken-rs** (real BPE tokenization, not heuristics) — `o200k_base` for GPT-5/4o/o-series, `cl100k_base` for older OpenAI models, `o200k_base` fallback for other providers
- **Works in both single-agent and orchestration mode**:
  - Single-agent: configure `[agent.scratchpad]` with top-level `memory_dir = "..."` — storage lands under `{memory_dir}/scratchpad/`, budget built from `[agent.llm].context_window`
  - Orchestration: `[agent.scratchpad]` provides defaults, `[orchestration.worker.<name>.scratchpad]` overrides per worker; top-level `memory_dir` also roots orchestration persistence (legacy `[orchestration.artifacts].memory_dir` still works as a fallback)
- **Per-worker budgets**: each worker gets a fresh `ContextBudget` scoped to its effective LLM (worker's `llm` override if set, otherwise `[agent.llm]`)
- Workers never share an "orchestrator-level" budget; budgets are created at `create_worker()` time and live on `Agent.scratchpad_budget`
- LLM-reported usage feedback (`input_tokens` + `output_tokens`) feeds into the budget as ground truth each turn — orchestration via `StreamItem::TurnUsage`, single-agent via the streaming hook's `on_stream_completion_response_finish`
- Per-call extraction limit (`max_extraction_tokens`, default 10k) prevents single reads from flooding context
- Auto-increased `turn_depth` when scratchpad is active (`turn_depth_bonus`, default 6) — applied in both single-agent and worker contexts
- `aura.scratchpad_usage` SSE event emitted per-agent with `agent_id`, `tokens_intercepted`, `tokens_extracted` — fires in both single-agent and orchestration contexts (lives in base `aura.*` namespace, not `aura.orchestrator.*`)
- Storage (orchestration): `{memory_dir}/{run_id}/iteration-{n}/scratchpad/`
- Storage (single-agent): `{memory_dir}/scratchpad/`
- `memory_dir` is a top-level TOML field shared by single-agent scratchpad and orchestration persistence

### Orchestration (Multi-Agent)
- Coordinator/worker architecture with DAG-based parallel task execution
- Per-worker LLM overrides: workers inherit `[agent.llm]` by default; `[orchestration.worker.<name>.llm]` overrides it (different model, same provider config). Resolved inline at worker construction (`worker.llm.as_ref().unwrap_or(&agent.llm)`)
- Dependency-aware multi-wave execution with iterative re-planning (`max_planning_cycles`)
- Three-way routing: direct answer, orchestrated plan, clarification
- `aura.orchestrator.*` SSE events for real-time visibility (see `docs/streaming-api-guide.md`)

### CLI (`aura-cli`)
- Interactive terminal client with REPL, one-shot mode, and conversation persistence
- **One-shot output contract** (`--query`): stdout is the **raw assistant response only** — no `●` markers, no markdown rendering, no tool-execution summaries, no response-summary header, no `backend.summarize` round-trip. Errors, permission prompts, and warnings go to stderr (with `error:` / `warning:` prefixes, no markers). Exit code 0 ⇒ stdout is the full response; non-zero ⇒ stderr explains and stdout is empty. The REPL retains rich formatting; the strict-output rules apply only to `--query` mode. See `crates/aura-cli/src/oneshot.rs`.
- **Two backends:** HTTP mode (default) and standalone mode (`--standalone --config`, builds agents in-process)
- Standalone mode requires `--features standalone-cli` at build time and explicit `--standalone` flag at runtime
- `--model` works in both modes: HTTP passes it as starting model; standalone matches against agent.name/agent.alias in configs
- `--system-prompt` works in both modes: standalone prompts for append/replace; HTTP prompts for AURA vs OpenAI-compatible service
- `--force` bypasses non-critical warnings (e.g. HTTP system-prompt in query mode)
- Local tool execution: Shell, Read, ListFiles, Update, SearchFiles, FindFiles, FileInfo
- CLI advertises local tools to the server with `--enable-client-tools`; the server attaches them only when `[agent].enable_client_tools = true` (filtered by `client_tool_filter` globs). **Single-agent configs only** — orchestrated configs drop the tools with a warning. No server-wide `--enable-client-tools` flag.
- **USE AT YOUR OWN RISK.** Enabling client-side tools is functionally equivalent to handing the LLM a shell prompt on the client machine — prompt injection, hallucination, and lack of sandboxing are real failure modes. See the prominent warnings in `README.md` and `crates/aura-cli/README.md` before enabling for any user-facing config.
- Permission system (`.aura/permissions.json`, formerly `settings.json`) with allow/deny glob rules. Discovered by walking up from `$PWD` to find the closest `.aura/`. **Project-scoped only** — no global `~/.aura/permissions.json`. Legacy `settings.json` is still read with a deprecation warning; new rules saved at the prompt land in `permissions.json` and migrate any existing legacy rules forward.
- CLI preferences live in `~/.aura/cli.toml` (global) and `<project>/.aura/cli.toml` (per-project override, walk-up discovered, merged on top of global per-field). Renamed from `~/.aura/config.toml` to avoid collision with AURA **agent** TOML configs; the old name is still read with a deprecation warning.
- `/model` command works in both modes — lists server models (HTTP) or loaded TOML configs (standalone)
- Env vars: `AURA_API_URL`, `AURA_API_KEY`, `AURA_MODEL`, `AURA_EXTRA_HEADERS`, `AURA_LOG_FILE`
- **Diagnostic logs**: opt-in via `--log-file <path>` / `AURA_LOG_FILE` / `cli.toml` `log_file` (precedence: CLI > env > project > global > none). Events are appended to the file (no rotation — user-managed) in **both REPL and one-shot mode**, so stdout stays a clean pipe. Default filter is `warn,aura=info,aura_cli=info,aura_config=info,rig::agent::prompt_request=info`; override with `RUST_LOG`.
- **OpenTelemetry (standalone only)**: when built with `--features standalone-cli` and run with `--standalone`, the CLI installs an OTel layer when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Trace shape mirrors the web server — `agent.stream` root span via `direct.rs`, with `agent.turn` / `mcp.tool_call` / `orchestration.*` nesting under it. CLI omits the HTTP-infrastructure spans (`chat_completions`, `streaming_completion`) since it has no HTTP layer.
- **Single shared tokio runtime**: `main` owns one `tokio::runtime::Runtime` and threads it into `Backend::from_config`, `run_oneshot`, and `run_repl`. `logging::init` runs inside `rt.enter()` so the OTLP gRPC exporter can call `Handle::current()` during `with_tonic()` construction; the `BatchSpanProcessor` worker lives on the same runtime that handles every subsequent request. `main` calls `aura::logging::shutdown_tracer()` via `rt.block_on(...)` before returning to flush buffered spans.
- SSE event parsing uses shared types from `aura-events` crate (not in `default-members`, build explicitly with `cargo build -p aura-cli`)
- See `crates/aura-cli/README.md` for full documentation

### Shared Event Types (`aura-events`)
- Lightweight crate defining `AuraStreamEvent` and `OrchestrationStreamEvent` enums
- Both `Serialize + Deserialize` — used by the web server (producer) and CLI (consumer)
- No agent, MCP, or provider dependencies — only `serde` and `serde_json`
- `ProgressToken` type uses a local wire-compatible definition by default; enables `rmcp-types` feature for direct rmcp interop (used by the `aura` crate)

## Environment Setup

```bash
export OPENAI_API_KEY="your-key"
export ANTHROPIC_API_KEY="your-key"  # Optional
export OPENROUTER_API_KEY="your-key" # Optional
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

## Commit and Contribution Rules

- **No AI co-authorship**: Never add `Co-Authored-By` lines for Claude or any AI assistant. Claude cannot accept the CLA.
- **Sign-off commits as the user**: Always sign off commits as the human user, not as Claude.
- **Commit message format**: [Conventional Commits](https://www.conventionalcommits.org/). First line must be entirely lowercase, no trailing period, under 72 characters. Use the body to explain **what** and **why**.
  Format: `<type>(<optional scope>): <description>`
  Types: `feat`, `fix`, `doc`, `style`, `refactor`, `perf`, `test`, `chore`, `ci`
  Breaking changes: add `!` after type/scope and include a `BREAKING CHANGE:` footer.
  If fixing an issue, include `Fixes: #<issue number>` in the footer.

## Documentation

- `README.md` - User-facing documentation
- `crates/aura-cli/README.md` - CLI usage, backends, features, and build instructions
- `CHANGELOG.md` - Auto-generated version history
- `docs/streaming-api-guide.md` - SSE streaming, custom events, and orchestration events
- `docs/ollama-guide.md` - Ollama configuration, fallback tool parsing, and local model guidance
- `docs/request-lifecycle.md` - Request flow, lifecycle, timeout, cancellation, and shutdown
- `docs/rig-tool-execution-order.md` - Tool execution order analysis
- `docs/rig-fork-changes.md` - Rig fork changes and rationale
- `docs/orchestration-tickets.md` - Epic ticket table, dependency graph, research references, implementation plan
