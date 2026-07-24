# AURA Web Server

> **Open Alpha** — AURA is under active development. APIs and configuration
> may change between releases. [Issues and feature requests](https://github.com/mezmo/aura/issues)
> are welcome — we'd love your feedback.

> **Part of the [AURA Project](../../README.md)** - A production-ready framework for building AI agents with declarative TOML configuration.

OpenAI-compatible web API server that exposes AURA agents through a standard chat completions endpoint.

Full usage reference (features, endpoints, request/response schemas, configuration, and deployment env vars): [docs.mezmo.com/aura/web-server-reference](https://docs.mezmo.com/aura/web-server-reference).

## Running Integration Tests

Integration tests verify streaming and tool execution functionality. Tests use a mock MCP server for tool execution and make LLM API calls to validate end-to-end behavior.

### Running Tests

The easiest path is `make test-integration-local`, which starts the server and mock MCP infrastructure with the environment the tests expect, runs the full suite, and tears everything down.

To run the pieces manually instead:

```bash
# 1. Start the server with test config (in one terminal). The full
# integration suite includes the A2A tests, which require A2A enabled
# and the agent-card URL pinned to the address the tests check:
AURA_ENABLE_A2A=true \
AURA_SERVER_URL=http://localhost:8080 \
CONFIG_PATH=crates/aura-web-server/tests/test-config.toml \
  cargo run --bin aura-web-server

# 2. Run tests (in another terminal). The test files are feature-gated,
# so a --features flag is required; without one, zero tests run.
# All integration tests
cargo test --package aura-web-server --features integration --no-fail-fast -- --test-threads=1

# Or a specific suite via its feature flag
cargo test --package aura-web-server --features integration-streaming --no-fail-fast -- --test-threads=1
```

The full list of suite feature flags, plus the `make test-integration-*` workflows that manage the test infrastructure for you, is in [DEVELOPMENT.md](../../DEVELOPMENT.md#testing).

### Prerequisites

- `OPENAI_API_KEY` environment variable (for LLM calls during tests)
