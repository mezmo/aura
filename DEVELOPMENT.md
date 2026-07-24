# Developing AURA

How to build, test, and navigate the AURA codebase. For the contribution process (CLA, commit conventions, pull requests), see [CONTRIBUTING.md](CONTRIBUTING.md).

## Prerequisites

- **Rust (nightly)**: the repo's `rust-toolchain.toml` installs the right toolchain automatically on first build.
- **Docker and Docker Compose**: required for integration tests and containerized builds.
- **Node.js 22+** (optional): used by CI tooling such as commit linting and release generation.

## Building from Source

1. Install Rust if you don't have it:

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. Clone and configure:

   ```bash
   git clone https://github.com/mezmo/aura.git
   cd aura
   cp examples/reference.toml config.toml
   cp .env.example .env
   ```

   Edit `.env` with your API keys. At minimum you'll need an `OPENAI_API_KEY` to run the server or integration tests. Keep secrets in environment variables and reference them in TOML using `{{ env.VAR_NAME }}`.

3. Build and verify:

   ```bash
   cargo build --workspace
   cargo test --workspace
   ```

4. Run the web server:

   ```bash
   cargo run --bin aura-web-server
   ```

## Project Structure

```text
aura/
├── crates/
│   ├── aura/                # Core agent builder library and orchestration
│   ├── aura-cli/            # Interactive terminal client (HTTP + standalone modes)
│   ├── aura-config/         # TOML parser and config loader
│   ├── aura-events/         # Shared SSE event types (lightweight, no agent deps)
│   ├── aura-telemetry/      # Anonymous CLI telemetry (see docs.mezmo.com/aura/telemetry)
│   ├── aura-telemetry-derive/ # Derive macros for aura-telemetry
│   ├── aura-test-utils/     # Shared testing utilities
│   └── aura-web-server/     # OpenAI-compatible HTTP/SSE server
├── compose/                 # Docker Compose (integration + orchestration overlays)
├── configs/                 # Integration test and example configurations
├── deployment/              # Helm charts and K8s manifests
├── docs/                    # Architecture and protocol documentation
├── examples/                # Example TOML configurations
│   ├── reference.toml       # Complete annotated configuration
│   ├── minimal/             # Bare minimum per-provider configs
│   └── complete/            # Full agent composition examples
├── scripts/                 # CI and utility scripts
└── tests/                   # Integration test fixtures and helpers
```

## Make Targets

Make targets are composed from modular includes under `.makefiles/` (rust, docker, node, aura, commitlint). Run `make help` for the full list. The most useful:

| Command                | Description                                    |
| ---------------------- | ---------------------------------------------- |
| `make build`           | Build all workspace crates                     |
| `make fmt`             | Format code with rustfmt                       |
| `make fmt-check`       | Check formatting (CI mode)                     |
| `make lint`            | Run clippy with warnings as errors             |
| `make ci`              | Run fmt-check and lint (the `test` hook is empty; run `cargo test --workspace` separately) |
| `make coverage`        | Run the test suite with code coverage          |
| `make lint-commits`    | Lint commits on the current branch against main |
| `make clean`           | Clean build artifacts                          |
| `make docker-build`    | Build the Docker image (full release)          |
| `make docker-test`     | Run the Docker build's lint/test stage         |
| `make start` / `make stop` | Start/stop the Docker Compose setup        |

## Testing

### Unit Tests

```bash
cargo test --workspace
```

Unit tests don't require any external services or API keys.

Config validation is covered by `cargo test -p aura-config`, including `test_all_shipped_configs_parse`, which validates every `.toml` file in `configs/`, `examples/`, and `quickstart.toml`.

### Integration Tests

Web server integration tests live under `crates/aura-web-server/tests/`. They verify end-to-end behavior through the web server, including LLM interaction and MCP tool execution. They **require a real `OPENAI_API_KEY`** because they make actual API calls to OpenAI. MCP tool execution is handled by mock servers, so no external tool APIs are called.

```bash
# Start local test infrastructure (mock MCP servers + AURA)
make test-integration-local-up

# Run the integration test suite
cargo test --package aura-web-server --features integration --no-fail-fast -- --test-threads=1

# Tear down when done
make test-integration-local-down

# Or do it all in one command:
make test-integration-local

# Other suites, same pattern (each also has a Docker Compose variant
# without the -local suffix, plus -local-up / -local-down helpers):
make test-integration-orchestration-local
make test-integration-sre-orchestration-local
make test-integration-scratchpad-local

# STDIO MCP tests (local only, no Compose/up/down variants; runs the
# aura crate's integration-stdio feature)
make test-integration-stdio-local

# Session-store tests (local only; needs Docker for an ephemeral Valkey,
# no other infra and no LLM key). To use an existing Redis/Valkey instead,
# set AURA_TEST_REDIS_URL and run the cargo command directly.
make test-integration-session-store-local
```

Integration tests run single-threaded (`--test-threads=1`) due to LLM API rate limits.

**Feature flags** (`crates/aura-web-server/Cargo.toml`) select specific suites:

| Flag                            | Suite                                                    |
| ------------------------------- | -------------------------------------------------------- |
| `integration`                   | All base integration tests (parent flag)                 |
| `integration-a2a`               | A2A protocol endpoints                                   |
| `integration-streaming`         | Streaming functionality                                  |
| `integration-header-forwarding` | MCP header forwarding                                    |
| `integration-mcp`               | MCP tool execution                                       |
| `integration-events`            | Custom `aura.*` events                                   |
| `integration-cancellation`      | Request cancellation                                     |
| `integration-progress`          | MCP progress notifications                               |
| `integration-orchestration`     | Orchestration (separate from parent `integration`)       |
| `integration-orchestration-sre` | SRE orchestration (requires k8s-sre-mcp server config)   |
| `integration-scratchpad`        | Scratchpad (separate from parent `integration`; requires scratchpad-test-mcp server config) |
| `integration-session-store`     | Redis/Valkey session store (separate from parent `integration`; requires a live Redis/Valkey via `AURA_TEST_REDIS_URL`) |
| `integration-vector`            | Vector store / RAG (requires external Qdrant)            |

Example, run only the streaming tests:

```bash
cargo test --package aura-web-server --features integration-streaming --no-fail-fast -- --test-threads=1
```

Detailed test guidance: [crates/aura-web-server/README.md](crates/aura-web-server/README.md).

### Writing Tests

- **Unit tests**: place `#[cfg(test)]` modules in the same file as the code they test. Preferred for internal/algorithmic changes.
- **Integration tests**: add to `crates/aura-web-server/tests/`, using the `aura-test-utils` crate for shared helpers. Consider adding or updating one when your change is user-facing.

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
- Header forwarding support (`headers_from_request`) for per-request MCP auth delegation. See [examples/reference.toml](examples/reference.toml) for a practical example.
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

### Further Reading

Worth reading before diving into the code:

- [Streaming API Guide](https://docs.mezmo.com/aura/streaming-api-guide): SSE protocol, event types, and client handling.
- [Request Lifecycle](https://docs.mezmo.com/aura/request-lifecycle): request flow, timeouts, cancellation, and shutdown.
- [docs/rig-fork-changes.md](docs/rig-fork-changes.md): why AURA uses a Rig.rs fork, what changed, and tool execution ordering (important for `tool_event_broker.rs`).
- [Tracing & Span Layout](https://docs.mezmo.com/aura/tracing-spans): OpenTelemetry span layout and trace parenting.
