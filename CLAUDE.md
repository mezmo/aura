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

# Build and run CLI (standalone mode — no server needed, default when --api-url absent)
cargo run -p aura-cli -- --config configs/my-agent.toml

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
- Intercepts large MCP tool outputs and saves them to disk instead of filling the context window; works in both single-agent and orchestration mode. Full usage/config docs: docs.mezmo.com/aura/scratchpad
- Code pointers: token-counter dispatch lives in `token_counter_for_provider` (`scratchpad/context_budget.rs`); per-agent budgets live on `Agent.scratchpad_budget`, created at `create_worker()` time; read tools resolve files under a per-agent **read root** distinct from the write-confined scratchpad dir (`ScratchpadStorage::with_read_root`)

### Orchestration (Multi-Agent)
- Coordinator/worker architecture with DAG-based parallel task execution
- Per-worker LLM overrides: workers inherit `[agent.llm]` by default; `[orchestration.worker.<name>.llm]` overrides it (different model, same provider config). Resolved inline at worker construction (`worker.llm.as_ref().unwrap_or(&agent.llm)`)
- Dependency-aware multi-wave execution with iterative re-planning (`max_planning_cycles`)
- Three-way routing: direct answer, orchestrated plan, clarification
- `aura.orchestrator.*` SSE events for real-time visibility (see docs.mezmo.com/aura/streaming-api-guide)

### CLI (`aura-cli`)
- Interactive terminal client with REPL, one-shot mode, and conversation persistence
- **One-shot output contract** (`--query`): stdout is the **raw assistant response only** — no `●` markers, no markdown rendering, no tool-execution summaries, no response-summary header, no `backend.summarize` round-trip. Errors, permission prompts, and warnings go to stderr (with `error:` / `warning:` prefixes, no markers). Exit code 0 ⇒ stdout is the full response; non-zero ⇒ stderr explains and stdout is empty. The REPL retains rich formatting; the strict-output rules apply only to `--query` mode. See `crates/aura-cli/src/oneshot.rs`.
- **Two backends:** standalone mode (default when `--api-url` absent) and HTTP mode (`--api-url`)
- Standalone mode is enabled by the `standalone-cli` default feature; `--standalone` flag overrides `AURA_API_URL` env var but is mutually exclusive with the `--api-url` flag. HTTP-only builds: `--no-default-features`
- `--model` works in both modes: HTTP passes it as starting model; standalone matches against agent.name/agent.alias in configs
- `--system-prompt` works in both modes: standalone prompts for append/replace; HTTP prompts for AURA vs OpenAI-compatible service
- `--force` bypasses non-critical warnings (e.g. HTTP system-prompt in query mode)
- Local tool execution: Shell, Read, ListFiles, Update, SearchFiles, FindFiles, FileInfo
- **USE AT YOUR OWN RISK.** CLI advertises local tools to the server with `--enable-client-tools`; the server attaches them only when `[agent].enable_client_tools = true` (filtered by `client_tool_filter` globs). Both sides must opt in; single-agent configs only. Functionally equivalent to handing the LLM a shell prompt on the client machine. Full risk model and protocol details: docs.mezmo.com/aura/client-side-tools
- Permission system (`.aura/permissions.json`, formerly `settings.json`) with allow/deny glob rules. Discovered by walking up from `$PWD` to find the closest `.aura/`. **Project-scoped only** — no global `~/.aura/permissions.json`. Legacy `settings.json` is still read with a deprecation warning; new rules saved at the prompt land in `permissions.json` and migrate any existing legacy rules forward.
- CLI preferences live in `~/.aura/cli.toml` (global) and `<project>/.aura/cli.toml` (per-project override, walk-up discovered, merged on top of global per-field). Renamed from `~/.aura/config.toml` to avoid collision with AURA **agent** TOML configs; the old name is still read with a deprecation warning.
- `/model` command works in both modes — lists server models (HTTP) or loaded TOML configs (standalone)
- Env vars: `AURA_API_URL`, `AURA_API_KEY`, `AURA_MODEL`, `AURA_EXTRA_HEADERS`, `AURA_LOG_FILE`
- **Diagnostic logs**: opt-in via `--log-file <path>` / `AURA_LOG_FILE` / `cli.toml` `log_file` (precedence: CLI > env > project > global > none). Events are appended to the file (no rotation — user-managed) in **both REPL and one-shot mode**, so stdout stays a clean pipe. Default filter is `warn,aura=info,aura_cli=info,aura_config=info,rig::agent::prompt_request=info`; override with `RUST_LOG`.
- **OpenTelemetry (standalone only)**: when running in standalone mode, the CLI installs an OTel layer when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Trace shape mirrors the web server — `agent.stream` root span via `direct.rs`, with `agent.turn` / `mcp.tool_call` / `orchestration.*` nesting under it. CLI omits the HTTP-infrastructure spans (`chat_completions`, `streaming_completion`) since it has no HTTP layer.
- **Single shared tokio runtime**: `main` owns one `tokio::runtime::Runtime` and threads it into `Backend::from_config`, `run_oneshot`, and `run_repl`. `logging::init` runs inside `rt.enter()` so the OTLP gRPC exporter can call `Handle::current()` during `with_tonic()` construction; the `BatchSpanProcessor` worker lives on the same runtime that handles every subsequent request. `main` calls `aura::logging::shutdown_tracer()` via `rt.block_on(...)` before returning to flush buffered spans.
- SSE event parsing uses shared types from the `aura-events` crate
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

Confirmed sequential as of Rig 0.28: the streaming handler `.await`s each tool call inline. See `docs/rig-fork-changes.md`.

## CI/CD

**Status**: Jenkins/Makefile complete, Helm charts and K8s manifests in `deployment/`

```bash
make build                  # Build release binary
cargo test --workspace      # Run all tests (the `make test` hook is empty)
make docker-build           # Build Docker image
make lint                   # Run clippy + fmt check
```

## Code Comment Conventions

- **Document behavior where it lives, not on data.** A comment describing *what
  happens* (how a value is produced, consumed, derived, or interpreted) belongs
  on the function/branch that implements it — not on a struct, field, enum
  variant, or other type definition. Type-level comments state only *what the
  value is* (its meaning/unit), never cross-referenced runtime behavior.
  - **The drift test — apply it to every type-level comment:** if the comment
    would have to change when code in *a different function* changes, it is in
    the wrong place. Move it to that function.
  - **Red-flag words on a type definition.** These almost always signal behavior
    narration that belongs elsewhere: "when", "if", "unless", "for … that",
    "absent/`None`/empty when", "set/populated when", "becomes", "used by",
    "consumed by", "so that", "which lets", "approximates", "exceeds",
    "falls back". Seeing one on a struct/field/enum is a prompt to move it.
  - **`Option`, enums, and defaults describe their own shape — don't narrate it.**
    The `Option` already says the value can be absent; the enum already lists its
    variants. Do *not* document *when* each state occurs (`None when …`,
    `Empty for …`) on the type — that condition lives at the code that sets it.
  - Bad (on a struct field): `execution_ms - tool_ms approximates LLM-thinking
    time`; `None for direct answers and runs that failed before any iteration`;
    `becomes the next iteration's planning_ms`. Each describes behavior owned by
    a consumer or a write site.
  - Good (on the field): `Plan ready → continuation-prompt entrypoint.` The
    derivation/interpretation lives at the code that computes or displays it.
- **Don't describe the same behavior in more than one place.** Pick the single
  site that implements it; from elsewhere, reference — don't restate. Duplicated
  prose drifts out of sync. If you find yourself writing something already
  documented at the implementing site, delete it rather than restating it.
- **Comment on current behavior, not change history.** Describe what the code
  does now, as if it were always this way — git records the diff, the comment
  should not. Do not phrase a comment relative to the past: avoid "prior we did
  XYZ, now we ABC", "(unchanged)", "legacy behavior", "now does X",
  "previously…", "still works as before", "moved from…", "keep current
  behavior". History-relative phrasing drifts out of sync as the code evolves
  and is noise to anyone who didn't witness the change — rewrite it as a
  present-tense statement of what the code does.
- **Before finishing any code change, run the drift test on every comment you
  added or touched that sits on a struct, field, enum, or variant.** For each,
  ask "would this change if a different function changed?" — if yes, move (don't
  copy) it onto the implementing code. Check the whole comment, not just its
  first clause: the anti-pattern often hides as a trailing "; …when…" or "; …
  becomes…" appended to an otherwise-correct definition.

## Commit and Contribution Rules

- **No AI co-authorship**: Never add `Co-Authored-By` lines for Claude or any AI assistant. Claude cannot accept the CLA.
- **Sign-off commits as the user**: Always sign off commits as the human user, not as Claude.
- **Commit message format**: [Conventional Commits](https://www.conventionalcommits.org/). First line must be entirely lowercase, no trailing period, under 72 characters. Use the body to explain **what** and **why**.
  Format: `<type>(<optional scope>): <description>`
  Types: `feat`, `fix`, `doc`, `style`, `refactor`, `perf`, `test`, `chore`, `ci`
  Breaking changes: add `!` after type/scope and include a `BREAKING CHANGE:` footer.
  If fixing an issue, include `Fixes: #<issue number>` in the footer.
  If the change relates to a tracked ticket, include `Ref: LOG-XXXXXX` in the footer; otherwise omit it (commitlint does not require it).

## Documentation

All user-facing documentation (quickstarts, configuration reference, feature guides, CLI reference, web server reference) lives externally at **docs.mezmo.com/aura** (repo: `mezmo/documentation`, content under `aura/`) — not in this repo. This repo's own docs are developer/contributor-facing only:

- `README.md` - User-facing only: what AURA is, quick start, and pointers to `DEVELOPMENT.md`/`CONTRIBUTING.md`/the hosted docs. No table of contents (GitHub generates one). No usage/configuration content, build-from-source, testing, architecture, or contributor content — those live at docs.mezmo.com/aura, in `DEVELOPMENT.md`, or in `CONTRIBUTING.md`.
- `DEVELOPMENT.md` - Developer-facing: prerequisites, building from source, project structure, Make targets, testing (unit + integration suites and feature flags), and the architecture overview.
- `CONTRIBUTING.md` - Contribution process only: CLA, workflow, code quality standards, commit conventions, PR and review process. Build/test details belong in `DEVELOPMENT.md`; link, don't duplicate.
- `docs/` - Architecture/rationale only, not usage guides: `docs/rig-fork-changes.md` (Rig fork changes and rationale). ADRs in `docs/adr/`, design docs in `docs/design/`.
- `crates/*/README.md` - Crate-specific build/test instructions only (e.g. `crates/aura-cli/README.md` covers building and testing the CLI; its usage docs are on the hosted site).
- `CHANGELOG.md` - Auto-generated version history; never edit by hand

When adding or changing a user-facing feature, update the corresponding page in the `mezmo/documentation` repo (`aura/` directory) — this repo's `docs/` folder is not the place for it.
