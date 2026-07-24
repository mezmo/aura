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

Running a binary needs a config — see the [quickstart guide](docs/quickstart.md) and the [configuration reference](docs/configuration-reference.md). To try AURA with no setup, use the Quick Start below.

## Quick Start

```bash
cp .env.example .env                                                  # set your LLM provider, model, and API key
docker compose up -d                                                  # starts Aura (orchestrator mode) + LibreChat + Phoenix
docker compose exec -it aura ./aura --api-url http://localhost:8080   # chat with the orchestrator from your terminal
```

Aura boots in **orchestrator mode**: a coordinator routes each request — answering simple ones directly and decomposing complex ones across specialized workers. The bundled `aura` connects to the in-container server automatically and renders the coordinator's plan and worker activity as it streams.

Prefer a browser? Open <http://localhost:3080> to chat in LibreChat, or <http://localhost:6006> to inspect traces in Phoenix.

**[Full quickstart guide](docs/quickstart.md)** — provider setup (OpenAI, Anthropic, Ollama, llama-server), adding MCP tools, enabling vector search, serving multiple agents, and troubleshooting.

### More Quickstarts

- **[Orchestration — Math MCP](examples/quickstart-orchestration-math/README.md)** — Multi-agent orchestration with coordinator/worker architecture
- **[Kubernetes SRE](examples/quickstart-k8s-sre/README.md)** — AI-powered SRE agent on KIND with Kubernetes and Prometheus MCP servers
- **[Example Configs](examples/README.md)** — Minimal per-provider configs and complete agent compositions

## Development and Contributing

- [DEVELOPMENT.md](DEVELOPMENT.md): building from source, project structure, Make targets, testing, and architecture.
- [CONTRIBUTING.md](CONTRIBUTING.md): how to contribute, including the CLA, commit conventions, and the PR process.

## Documentation

**Configuration**

- [docs/configuration-reference.md](docs/configuration-reference.md): complete TOML field reference — agent, LLM providers, MCP, vector stores, scratchpad, skills, orchestration — plus multiple-agent serving and session store env vars.
- [docs/quickstart.md](docs/quickstart.md): getting started guide — setup, customization, architecture, and troubleshooting.

**Subsystems**

- [docs/streaming-api-guide.md](docs/streaming-api-guide.md): SSE protocol guide, event taxonomy, tool result modes, custom `aura.*` events, orchestration events, and client examples.
- [docs/a2a-implementation.md](docs/a2a-implementation.md): A2A protocol endpoints, transport modes (REST and JSON-RPC), task lifecycle, and testing examples.
- [docs/hitl.md](docs/hitl.md): human approval gates for orchestration worker tool calls, including webhook and conversational routes.
- [docs/scratchpad.md](docs/scratchpad.md): context window management — large tool output interception, exploration tools, and token budgeting.
- [docs/skills.md](docs/skills.md): on-demand agent instructions — the Agent Skills format, discovery, and orchestration inheritance.
- [docs/client-side-tools.md](docs/client-side-tools.md): client-side tool passthrough — risk model, protocol mechanics, and server/CLI configuration.
- [docs/request-lifecycle.md](docs/request-lifecycle.md): request flow diagram, lifecycle, timeout, cancellation, and shutdown behavior.
- [docs/ollama-guide.md](docs/ollama-guide.md): Ollama configuration, fallback tool parsing, and local model guidance.
- [docs/tracing-spans.md](docs/tracing-spans.md): enabling OpenTelemetry, span layout, OpenInference span kinds, and trace parenting for both single-agent and orchestration modes.
- [docs/telemetry.md](docs/telemetry.md): anonymous, opt-out CLI telemetry — the three-state consent model, exactly what is and isn't collected, kill switches, and how to audit it.
- [docs/rig-fork-changes.md](docs/rig-fork-changes.md): Rig fork changes, tool execution order, and rationale.

**Reference**

- [CHANGELOG.md](CHANGELOG.md): release and version history.
- [docs/breaking-changes/20260421-llm-under-agent.md](docs/breaking-changes/20260421-llm-under-agent.md): breaking configuration changes from 21 April 2026 — `[llm]` moved under `[agent.llm]` and per-worker LLM overrides.
- [docs/breaking-changes/20260410-agent-llm-toml-configuration.md](docs/breaking-changes/20260410-agent-llm-toml-configuration.md): breaking configuration changes from 10 April 2026 — field migrations from `[agent]` to `[llm]` and Ollama parameter consolidation.

**Crate-specific**

- [crates/aura-cli/README.md](crates/aura-cli/README.md): the `aura` CLI — REPL, one-shot mode, permissions, local tools.
- [crates/aura-web-server/README.md](crates/aura-web-server/README.md): `aura-web-server` — endpoints, deployment env vars, integration tests.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
