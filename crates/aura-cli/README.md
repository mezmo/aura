# AURA CLI

A fast, interactive terminal client for chat completions with tool execution. Built primarily as the command line interface for [AURA by Mezmo](https://mezmo.com/aura), but works with **any OpenAI-compatible API** — plug in your own models, agents, or LLM endpoints.

Full usage reference (Quick Start, `aura init`, backends, environment variables, CLI flags, REPL commands, client-side tools, permissions, logging, and more): [docs.mezmo.com/aura/cli-reference](https://docs.mezmo.com/aura/cli-reference).

---

## Building & Testing

```bash
# Build (default — standalone + HTTP)
cargo build -p aura-cli

# Build (HTTP-only — lightweight, no agent dependencies)
cargo build -p aura-cli --no-default-features

# Run tests
cargo test -p aura-cli                       # default (standalone + HTTP)
cargo test -p aura-cli --no-default-features  # HTTP-only path

# Clippy
cargo clippy -p aura-cli --all-targets
cargo clippy -p aura-cli --no-default-features --all-targets

# Run directly
cargo run -p aura-cli -- --config agent.toml                    # standalone (default)
cargo run -p aura-cli -- --api-url "http://localhost:8080"      # HTTP mode
```
