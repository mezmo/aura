# AURA

[![Join the AURA community on Slack](https://img.shields.io/badge/Slack-Join%20the%20community-4A154B?logo=slack&logoColor=white "Join the AURA community on Slack")](https://mezmo.com/r/slack-aura)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue "Apache License, Version 2.0")](LICENSE)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-orange?logo=rust "Built with Rust 1.85 or later")](https://www.rust-lang.org)
[![MCP compatible](https://img.shields.io/badge/MCP-compatible-green "Model Context Protocol compatible")](https://modelcontextprotocol.io)

AURA is an agentic harness that turns LLM models into a reliable, autonomous service capable of executing real SRE work. AURA provides the guardrails, API servers, state management, authentication, streaming, error handling, and tool integrations necessary to run AI SRE agents safely in production.

With AURA you can:

- Run entirely on your own infrastructure, including air-gapped and highly-regulated environments
- Define orchestrated clusters of production agents in one simple, human-readable TOML file: models, per-agent prompts, tools, and guardrails
- Run on the LLM backends you already use (OpenAI, Anthropic, Bedrock, Gemini, Ollama, OpenRouter), switch providers with a one-section edit, or mix models per worker
- Give agents real tools: point a config at any MCP server (HTTP streamable, SSE, or STDIO) and its tools are discovered and callable at runtime
- Require human approval (webhook or in-conversation) before sensitive tool calls execute
- Survive huge tool outputs without context floods: oversized results are parked on disk and the agent pulls in only the slices it needs
- Fan complex requests out to user-defined specialist workers: a coordinator plans a task DAG, runs independent tasks in parallel, and consolidates the results
- Ground answers in your own data with vector search (Qdrant, AWS Bedrock Knowledge Base) and teach agents task-specific procedures with on-demand skills
- Chat from the terminal with or without a server: the CLI runs agents in-process from a config, or connects to any OpenAI-compatible API
- Interoperate with other agents over the A2A protocol, or embed the Rust core directly in your own application
- Serve every agent as an OpenAI-compatible API, so existing clients and SDKs (LibreChat, OpenWebUI, …) work unchanged

## Install

Install the `aura` CLI and `aura-web-server` binaries (Linux/macOS, amd64/arm64):

```bash
curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
```

| Variable | Default | Description |
| --- | --- | --- |
| `AURA_VERSION` | `latest` | Release version to install |
| `AURA_INSTALL` | `~/.local/bin` | Install directory |
| `AURA_COMPONENT` | `all` | Which binary: `all`, `server`, or `cli` |
| `AURA_REQUIRE_CHECKSUM` | `0` | `1` to fail when a release checksum is missing, `0` to warn and continue |

Running a binary needs a config — see the [quickstart guide](https://docs.mezmo.com/aura/quickstart) and the [configuration reference](https://docs.mezmo.com/aura/configuration-reference). To try AURA with no setup, use the Quick Start below.

## Quick Start

```bash
cp .env.example .env                                                  # set your LLM provider, model, and API key
docker compose up -d                                                  # starts Aura (orchestrator mode) + LibreChat + Phoenix
docker compose exec -it aura ./aura --api-url http://localhost:8080   # chat with the orchestrator from your terminal
```

Aura boots in **orchestrator mode**: a coordinator routes each request — answering simple ones directly and decomposing complex ones across specialized workers. The bundled `aura` connects to the in-container server automatically and renders the coordinator's plan and worker activity as it streams.

Prefer a browser? Open <http://localhost:3080> to chat in LibreChat, or <http://localhost:6006> to inspect traces in Phoenix.

**[Full quickstart guide](https://docs.mezmo.com/aura/quickstart)** — provider setup (OpenAI, Anthropic, Ollama, llama-server), adding MCP tools, enabling vector search, serving multiple agents, and troubleshooting.

### More Quickstarts

- **[Orchestration — Math MCP](https://docs.mezmo.com/aura/quickstart-orchestration-math)** — Multi-agent orchestration with coordinator/worker architecture
- **[Kubernetes SRE](https://docs.mezmo.com/aura/quickstart-k8s-sre)** — AI-powered SRE agent on KIND with Kubernetes and Prometheus MCP servers
- **[Example Configs](https://docs.mezmo.com/aura/example-configs)** — Minimal per-provider configs and complete agent compositions

## Development and Contributing

- [DEVELOPMENT.md](DEVELOPMENT.md): building from source, project structure, Make targets, testing, and architecture.
- [CONTRIBUTING.md](CONTRIBUTING.md): how to contribute, including the CLA, commit conventions, and the PR process.

## Documentation

Full user-facing documentation — quickstarts, configuration reference, feature guides, CLI reference, and web server reference — lives at **[docs.mezmo.com/aura](https://docs.mezmo.com/aura)**.

This repo's own docs are developer/contributor-facing only (see [Development and Contributing](#development-and-contributing) above for build/test/contribution docs):

- [docs/rig-fork-changes.md](docs/rig-fork-changes.md): Rig fork changes, tool execution order, and rationale.
- [docs/adr/](docs/adr/): architecture decision records.
- [docs/design/](docs/design/): design and implementation notes.
- [CHANGELOG.md](CHANGELOG.md): release and version history.
- [crates/aura-cli/README.md](crates/aura-cli/README.md): building and testing the `aura` CLI.
- [crates/aura-web-server/README.md](crates/aura-web-server/README.md): running the web server's integration test suite.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
