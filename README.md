<div align="center">
  <h1>AURA</h1>
  <p><strong>Build and run reliable AI agents anywhere.</strong></p>
  <p>
    <a href="#quick-start"><strong>Quick Start</strong></a> ·
    <a href="#integrations"><strong>Integrations</strong></a> ·
    <a href="#explore-aura"><strong>Explore</strong></a> ·
    <a href="https://mezmo.com/r/slack-aura"><strong>Community</strong></a>
  </p>

  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue" alt="Apache License, Version 2.0"></a>
  <a href="https://modelcontextprotocol.io"><img src="https://img.shields.io/badge/MCP-compatible-green" alt="Model Context Protocol compatible"></a>
</div>

AURA is a self-hosted runtime for turning the LLMs you already use into agents that can safely work with real systems. Define one agent or an orchestrated agent swarm in readable TOML, connect tools through MCP, run locally as a single binary, or serve an OpenAI-compatible API.

## Quick Start

Install AURA on Linux or macOS with the [install script](scripts/install.sh):

```bash
curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
```

Create a ready-to-run local agent:

```bash
aura init        # Choose an LLM provider & initial model, and write the initial config file
```

Start the agent:

```bash
aura
```

## Why AURA

- **Run entirely on your own infrastructure.** Deploy AURA in air-gapped and highly regulated environments.
- **Define complete agent systems in readable TOML.** Configure orchestrated agent swarms, models, per-agent prompts, tools, and guardrails in one file.
- **Use the LLM backends you already have.** Run OpenAI, Anthropic, Bedrock, Gemini, Ollama, or OpenRouter; switch providers with a one-section edit or mix models across workflows.
- **Give agents real tools.** Connect any compatible MCP server over Streamable HTTP, SSE, or STDIO; its tools are discovered and callable at runtime.
- **Keep humans in control.** Require webhook or in-conversation approval before sensitive tool calls execute.
- **Handle huge tool outputs.** Park oversized results on disk and let agents retrieve only the slices they need, avoiding context floods.
- **Fan work out to specialist agents.** A coordinator plans a task DAG, runs independent tasks in parallel, and consolidates the results.
- **Inspect every run.** AURA can export [OpenTelemetry traces](docs/tracing-spans.md) for requests, LLM turns, tool calls, and orchestration decisions, giving you an end-to-end view of how each result was produced.
- **Ground your agents in your data.** Use vector search with Qdrant or AWS Bedrock Knowledge Bases.
- **Give agents reusable skills.** Add [Agent Skills](https://agentskills.io) directories to a config; agents load task-specific instructions and supporting files only when needed.
- **Chat locally or through a server.** Run agents in-process from a config or connect the CLI to any OpenAI-compatible API.
- **Interoperate or embed.** Connect with other agents over A2A or embed AURA's Rust core directly in your application.
- **Use existing clients and SDKs.** Serve every agent through an OpenAI-compatible API so clients such as LibreChat and OpenWebUI work unchanged.

## Integrations

Through compatible [MCP](https://modelcontextprotocol.io) servers, AURA agents can work with:

| Integration | What agents can do |
| --- | --- |
| AWS | Inspect cloud resources, logs, metrics, and operational state |
| Azure | Inspect cloud resources, deployments, monitoring, and operational state |
| Confluence | Search and maintain operational runbooks |
| Datadog | Query metrics, monitors, dashboards, and traces |
| Docker | Inspect containers, images, logs, and runtime state |
| GCP | Inspect cloud resources, logs, metrics, and operational state |
| GitHub | Search code and work with repositories, issues, and pull requests |
| GitLab | Search code and work with repositories, issues, merge requests, and pipelines |
| Jira | Search and update issues, projects, and workflows |
| Kafka | Inspect clusters, topics, consumer groups, and message flows |
| Kubernetes | Inspect clusters, workloads, events, and logs |
| Mezmo | Analyze logs, exports, and telemetry pipelines |
| New Relic | Query metrics, logs, traces, alerts, and dashboards |
| Notion | Search and maintain operational runbooks |
| PagerDuty | Investigate incidents, on-call schedules, and escalations |
| Prometheus | Query metrics and alert status |

## Ways to Run AURA

- **As a local chat assistant.** Run AURA interactively from your terminal.
- **As a service.** Run the `aura-web-server` daemon and connect it to monitoring systems to trigger agent workflows.
- **As a container.** Run the published [`mezmo/aura`](https://hub.docker.com/r/mezmo/aura) Docker image.
- **As a Kubernetes workload.** Deploy AURA with the [included Helm chart](deployment/helm/aura).
- **As a library.** Embed AURA's Rust core directly in your own application.

## Explore AURA

- [Browse agent configurations and advanced quickstarts](examples/README.md)
- [Browse the annotated configuration reference](examples/reference.toml)
- [Build an orchestrated multi-agent workflow](examples/quickstart-orchestration-math/README.md)
- [Run a Kubernetes SRE agent](examples/quickstart-k8s-sre/README.md)
- [Learn the full AURA CLI](crates/aura-cli/README.md)
- [Use AURA's streaming API](docs/streaming-api-guide.md)
- [Develop AURA](DEVELOPMENT.md) or [contribute](CONTRIBUTING.md)

## Community

Join the [AURA Slack community](https://mezmo.com/r/slack-aura) to ask questions, share what you are building, and help shape the roadmap.

## License

AURA is licensed under the [Apache License, Version 2.0](LICENSE).
