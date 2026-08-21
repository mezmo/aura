<div align="center">
  <h1>AURA</h1>
  <p><strong>AURA is a production-tested SRE agent platform you can deploy in minutes.</strong></p>
  <img src="https://res.cloudinary.com/dmoknxebz/image/upload/v1785777937/github-header-aura-20260803.webp" alt="AURA investigating an incident from the terminal" width="720">
  <p><em>In this demo, AURA investigates a payment failure by correlating Mezmo traces and logs, Prometheus latency, and Kubernetes deployment history. Three specialist agents identify an N+1 regression introduced by productcatalogservice 1.13.2, recommend rolling back to 1.13.1, and provide recovery checks and engineering follow-up.</em></p>
  <p>
    <a href="#quick-start"><strong>Quick Start</strong></a> ·
    <a href="#integrations"><strong>Integrations</strong></a> ·
    <a href="https://docs.mezmo.com/aura"><strong>Documentation</strong></a> ·
    <a href="https://github.com/mezmo/aura/issues/views/4715"><strong>Roadmap</strong></a> ·
    <a href="#explore-aura"><strong>Explore</strong></a> ·
    <a href="https://mezmo.com/r/slack-aura"><strong>Community</strong></a>
  </p>

  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue" alt="Apache License, Version 2.0"></a>
  <a href="https://modelcontextprotocol.io"><img src="https://img.shields.io/badge/MCP-compatible-green" alt="Model Context Protocol compatible"></a>
  <a href="https://cloudsmith.com"><img src="https://img.shields.io/badge/OSS%20hosting%20by-cloudsmith-blue?logo=cloudsmith" alt="OSS hosting by Cloudsmith"></a>
</div>

Connect your stack through guided setup, and AURA's preconfigured team of agents starts investigating incidents using the models you already rely on. From there, customize existing agents or add new ones to fit your infrastructure and SRE workflows.

AURA handles the guardrails, APIs, state management, streaming, failure handling, and observability needed to connect AI models to production tools within operator-defined boundaries.

## Proven in Production

AURA began as Mezmo’s internal harness for operating our own SaaS. Our engineering and SRE teams still use it for production operations today, and teams outside Mezmo run AURA in their own production environments.

AURA also powers thousands of agent sessions each month in Mezmo’s hosted observability platform.

## Quick Start

Install AURA on Linux or macOS with the [install script](scripts/install.sh). It downloads published release artifacts and verifies their checksums.

```bash
curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
```

Create a ready-to-run local agent:

```bash
aura init        # Choose an LLM provider and initial model, and write the initial config file
```

Start the agent:

```bash
aura
```

## Why AURA

- **Operate inside your own security boundary.** Run AURA in your infrastructure, including air-gapped and highly regulated environments, with control over model, tool, storage, and telemetry destinations.
- **Define the entire agent system in reviewable TOML.** Keep models, specialist agent teams, per-agent prompts, tools, approval policies, and guardrails together in configuration that can be versioned and reviewed.
- **Use different model providers without rebuilding workflows.** Run OpenAI, Anthropic, Bedrock, Gemini, Ollama, or OpenRouter; switch providers with a configuration change or assign different models to different roles.
- **Put sensitive actions behind human approval.** Require webhook or in-conversation approval before configured tool calls execute, with denial, timeout, and transport failures handled fail-closed.
- **Trace every model, tool, and orchestration decision.** Export [OpenTelemetry traces](https://docs.mezmo.com/aura/tracing-spans) for requests, LLM turns, tool calls, and multi-agent execution so each result can be investigated end to end.

## Extensible Runtime

- **Connect tools and RAG.** Discover tools from compatible MCP servers over Streamable HTTP, SSE, or STDIO, and ground agents through Qdrant or AWS Bedrock Knowledge Bases.
- **Build multi-agent workflows.** Coordinate specialist agent teams with dependency-aware task execution while parking oversized tool results on disk for selective retrieval.
- **Add reusable Agent Skills.** Load task-specific [Agent Skills](https://agentskills.io) instructions and supporting files only when they are needed.
- **Interoperate or embed.** Connect AURA with other agents over A2A or embed its Rust core directly in your application.
- **Use existing clients and SDKs.** Serve agents through an OpenAI-compatible API so clients such as LibreChat and OpenWebUI work unchanged, or run them locally through the AURA CLI.

## Production Safety

Production controls define an operator-managed boundary around AURA:

- Runs in your infrastructure, including air-gapped environments when model providers and MCP servers are locally reachable.
- AURA sends agent prompts and tool data to the model providers, MCP servers, approval services, storage backends, and tracing destinations you configure. Enabled client-side tools and STDIO processes can initiate additional network traffic unless system-level network policy prevents it. Mezmo CLI product telemetry is separately disclosed and controlled.
- Sensitive tool calls can require explicit human approval.
- Credentials supplied through environment variables or secret mounts remain outside prompts only when referenced from designated authentication fields; environment substitution in prompt-bearing fields places their values into model context.
- Tool, model, and orchestration activity can be exported as [OpenTelemetry traces](https://docs.mezmo.com/aura/tracing-spans).

See the complete [security and data-handling model](SECURITY.md), including telemetry defaults, permission boundaries, prompt-injection risks, and supply-chain verification.

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
- **As a service.** Run `aura webserver` as a daemon and connect it to monitoring systems to trigger agent workflows.
- **As a container.** Run the published [`mezmo/aura`](https://hub.docker.com/r/mezmo/aura) Docker image.
- **As a Kubernetes workload.** Deploy AURA with the [included Helm chart](deployment/helm/aura).
- **As a library.** Embed AURA's Rust core directly in your own application.

## Explore AURA

- [Browse agent configurations and advanced quickstarts](https://docs.mezmo.com/aura/example-configs)
- [Browse the annotated configuration reference](https://docs.mezmo.com/aura/configuration-reference)
- [Build an orchestrated multi-agent workflow](https://docs.mezmo.com/aura/quickstart-orchestration-math)
- [Run a Kubernetes SRE agent](https://docs.mezmo.com/aura/quickstart-k8s-sre)
- [Learn the full AURA CLI](https://docs.mezmo.com/aura/cli-reference)
- [Use AURA's streaming API](https://docs.mezmo.com/aura/streaming-api-guide)
- [Develop AURA](DEVELOPMENT.md) or [contribute](CONTRIBUTING.md)

## Community

Join the [AURA Slack community](https://mezmo.com/r/slack-aura) to ask questions, share what you are building, and help shape the roadmap.

## Package Hosting

Package repository hosting is graciously provided by [Cloudsmith](https://cloudsmith.com), the only fully hosted, cloud-native, universal package management solution — letting your organization create, store, and share packages in any format, to any place, with total confidence.

## License

AURA is licensed under the [Apache License, Version 2.0](LICENSE).
