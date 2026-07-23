# README content migration

This ledger records where material removed or condensed by the README time-to-value rewrite belongs. It is a planning source for the full AURA documentation site now under development, not a public navigation page.

The README must link only to pages that exist. Proposed destinations below are intentionally not linked from the README.

## Disposition labels

- **Keep in README** — concise product or onboarding content remains on the repository landing page.
- **Link now** — a focused repository document already owns the topic.
- **Future docs** — the topic belongs on the documentation site but has no complete destination yet.
- **Technical review** — preserve the source until an owner verifies current behavior and migration accuracy.
- **Delete** — obsolete or duplicative material should not be migrated.

## Content ledger

| Current README content | Disposition | Destination | Temporary source or link | Migration notes |
| --- | --- | --- | --- | --- |
| Product description and exhaustive capability list | Keep in README | README introduction and Why AURA | `README.md` | Rewrite around outcomes; make one-file agent swarms explicit and keep deployment freedom, MCP, guardrails, context management, and provider choice. |
| Installer command | Keep in README | README Quick Start | `scripts/install.sh` | Keep the one-line command. The script prefers Homebrew on macOS and otherwise installs release binaries. |
| Installer environment-variable table | Future docs | Installation / CLI installation | `scripts/install.sh` | Document `AURA_VERSION`, `AURA_INSTALL`, `AURA_COMPONENT`, `AURA_REQUIRE_CHECKSUM`, and `AURA_NO_BREW`. |
| Docker Compose quick start | Technical review | Ways to run AURA / Docker Compose | `docs/quickstart.md`, `docker-compose.yml` | The current quick start is no longer the primary path and is known to be inaccurate. Verify before migration. |
| Orchestration and Kubernetes quickstart links | Link now | README Explore section | `examples/quickstart-orchestration-math/README.md`, `examples/quickstart-k8s-sre/README.md` | Keep as optional next steps after local first value. |
| Web API server commands and option table | Future docs | Ways to run AURA / Web server | `README.md` before rewrite; CLI `--help` | Re-verify flags, environment variables, defaults, proxy guidance, and production recommendations. |
| Health, model-list, chat-completion, and streaming curl examples | Link now | HTTP and streaming API | `docs/streaming-api-guide.md` | The future docs site should split basic API use from the complete SSE event reference. |
| A2A overview, enablement, endpoints, and proxy warning | Link now | A2A | `docs/a2a-implementation.md` | Keep the disabled-by-default and `AURA_SERVER_URL` requirements prominent in the focused guide. |
| Client-side tools security warning and protocol walkthrough | Link now | CLI local tools and security | `crates/aura-cli/README.md#client-side-tools` | Preserve the full prompt-injection, host-privilege, permission-boundary, and orchestration limitations. Do not condense these warnings into an unsafe quick-start step. |
| Recent breaking configuration changes | Link now | Upgrade guides | `docs/breaking-changes/` | Future docs should provide a single upgrade-guide index. |
| Multiple config files, aliases, hidden agents, and model selection | Future docs | Configuration / Serving multiple agents | `examples/README.md`, `examples/reference.toml` | Consolidate behavior and examples; verify directory-loading defaults. |
| Configuration section overview and minimal TOML | Future docs | Configuration / Agent schema | `examples/reference.toml` | The annotated reference remains the temporary source of truth. |
| MCP transports, commands, arguments, header forwarding, and tool-name collisions | Future docs | Tools / Connect MCP servers | `examples/reference.toml`, issue `#186` | Explain HTTP streamable, SSE, and STDIO tradeoffs and preserve the duplicate-name caveat. |
| Config validation commands | Future docs | Configuration / Validate a config | `crates/aura-cli/README.md`, server CLI `--help` | Prefer installed-binary commands over source-build commands on the future site. |
| Orchestration overview, configuration, execution loop, and field tables | Future docs | Orchestration | `examples/quickstart-orchestration-math/README.md`, `configs/example-math-orchestration.toml` | Build a conceptual guide plus separate configuration reference. |
| Human-in-the-loop approval gates | Link now | Human approval | `docs/hitl.md` | Preserve current webhook, conversational, SSE, and scope limitations. |
| Scratchpad and large-result context management | Technical review | Context management / Scratchpad | `README.md` before rewrite, `CLAUDE.md` | Create a focused conceptual and configuration guide; verify thresholds, storage paths, budgets, and event names first. |
| On-demand skills | Future docs | Agent composition / Skills | `README.md` before rewrite, `examples/reference.toml` | Explain discovery, Agent Skills format, source precedence, path resolution, and orchestration inheritance. |
| Vector stores and retrieval-augmented generation | Future docs | Knowledge / Vector search | `examples/reference.toml`, `docs/quickstart.md` | Split Qdrant and Bedrock Knowledge Base setup; explain embedding providers and how stores attach to agents, coordinators, and workers. |
| Ollama setup and caveats | Link now | Local models / Ollama | `docs/ollama-guide.md` | Retain the focused guide until the docs site exists. |
| OpenTelemetry and OpenInference observability | Link now | Observability | `docs/tracing-spans.md`, `docs/telemetry.md` | Separate product tracing from anonymous CLI telemetry on the future site. |
| Quickstart architecture diagram and component descriptions | Technical review | Architecture / Runtime topology | `docs/quickstart.md`, `DEVELOPMENT.md` | Rebuild around current local, server, container, and Kubernetes modes instead of preserving the Docker-only diagram. |
| Quickstart troubleshooting | Future docs | Troubleshooting | `docs/quickstart.md` | Re-verify startup, model discovery, host-networking, reset, and diagnostic guidance before publishing. |
| Development and contribution links | Keep in README | README Explore and Community | `DEVELOPMENT.md`, `CONTRIBUTING.md` | Keep only short routes; all procedural details remain in their owning files. |
| Long documentation index | Keep in README | README Explore | Existing focused docs under `docs/` | Replace the exhaustive index with links organized around likely next actions. |
| License | Keep in README | README License | `LICENSE` | Keep one sentence and a link. |

## Required future docs page: Ways to run AURA

Create a decision-oriented page that explains when and how to run AURA as:

- An interactive local helper through the standalone `aura` CLI.
- A long-running daemon in self-managed infrastructure.
- A Kubernetes pod using the maintained deployment assets.
- A standalone container.
- The `aura-web-server` OpenAI-compatible service.
- An embedded Rust library where direct integration is preferable.

The page should compare prerequisites, process lifetime, configuration loading, networking, scaling, observability, upgrade strategy, and appropriate use cases. It should lead with AURA's self-hosted, self-contained-binary portability rather than presenting one deployment mode as universally preferred.

## Follow-up hygiene

When a future destination is published:

1. Move or rewrite the preserved material into that page.
2. Replace temporary repository links where the docs site offers a better route.
3. Update the corresponding row to **Link now** and add the live destination.
4. Remove stale source notes only after the new page has been technically reviewed.
