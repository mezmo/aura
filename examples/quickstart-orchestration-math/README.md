# Aura Orchestration Quickstart — Math MCP

One command to spin up Aura in **orchestration mode** with a math tool server:

- **Aura** — the AI agent server running in orchestration mode (`mezmo/aura:orchestration`)
- **Math MCP** — an MCP server providing arithmetic, statistics, and trigonometry tools
- **LibreChat** — a ChatGPT-style web UI connected to Aura
- **Phoenix** — an LLM trace viewer for inspecting every tool call, prompt, and token

## How Orchestration Mode Works

In orchestration mode Aura routes each request through a **coordinator** that decides how to respond:

- **Direct answer** — simple, single-step requests are answered immediately
- **Worker dispatch** — complex or multi-step problems are decomposed into a plan and delegated to specialized workers, each with access to a filtered subset of tools
- **Clarification** — vague requests prompt the coordinator to ask for more detail

This quickstart defines three workers backed by the math MCP server:

| Worker | Tools | Example task |
|---|---|---|
| `arithmetic` | add, subtract, multiply, division, modulo, floor, ceiling, round, sum | "Multiply 12 by 7, then subtract 4" |
| `statistics` | mean, median, mode, min, max | "Find the mean and median of [3, 7, 2, 9, 5]" |
| `trigonometry` | sin, cos, tan, arcsin, arccos, arctan, radiansToDegrees, degreesToRadians | "Compute sin(30°) and cos(60°)" |

## Setup

### 1. Add your API key

```bash
cp .env.example .env
```

Edit `.env` and paste your OpenAI (or Anthropic) API key.

### 2. Start everything

```bash
docker compose up --build
```

The `--build` flag is required on first run to build the math MCP image.

### 3. Open the UIs

| Service | URL | Description |
|---------|-----|-------------|
| LibreChat | <http://localhost:3080> | Chat with your agent |
| Phoenix | <http://localhost:6006> | Inspect LLM traces |
| Aura API | <http://localhost:3000> | OpenAI-compatible API |

**LibreChat first-time setup:** Create your user account on the signup page. The agent model is pre-configured as "Aura".

## Try It Out

Once everything is up, try these prompts in LibreChat:

**Direct answer** (no workers dispatched):
> What is 2 + 2?

**Multi-step orchestration** (coordinator dispatches arithmetic worker):
> Calculate (3 + 7) × 2, then subtract 5 from the result.

**Mixed domain** (coordinator dispatches arithmetic + statistics workers):
> Multiply each of [1, 2, 3, 4] by 3, then find the mean of the results.

**Clarification trigger**:
> Compute the thing.

Watch Phoenix at <http://localhost:6006> to see the full trace of coordinator planning and worker execution.

## Customize

Edit `config.toml` and restart Aura:

```bash
docker compose restart aura
```

### Switch LLM provider

Uncomment the Anthropic block in `config.toml` and set `ANTHROPIC_API_KEY` in `.env`.

### Adjust routing behavior

```toml
[orchestration]
allow_direct_answers = true   # set false to always dispatch workers
allow_clarification = true    # set false to never ask for clarification
max_planning_cycles = 2       # coordinator retries on low-quality plans
quality_threshold = 0.7       # minimum plan quality score (0.0–1.0)
```

### Add or modify workers

Each `[orchestration.worker.<name>]` section defines a worker. The `mcp_filter` list controls which tools from the math MCP server that worker can use.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  docker compose                                         │
│                                                         │
│  ┌────────────┐     ┌──────────────────────────────┐    │
│  │ LibreChat  │────▶│   Aura (orchestration mode)  │    │
│  │   :3080    │     │           :3000              │    │
│  └─────┬──────┘     └──────┬───────────────────────┘    │
│        │                   │                            │
│  ┌─────┴──────┐     ┌──────▼──────┐   ┌────────────┐   │
│  │  MongoDB   │     │  Math MCP   │   │  Phoenix   │   │
│  │ (LibreChat │     │    :8081    │   │   :6006    │   │
│  │  storage)  │     │             │   │            │   │
│  └────────────┘     └─────────────┘   └────────────┘   │
└─────────────────────────────────────────────────────────┘
         ▲                                    ▲
         │                                    │
      Browser                           OTel traces
```

- **LibreChat** sends chat requests to Aura's OpenAI-compatible `/v1/chat/completions` endpoint
- **Aura** runs a coordinator LLM that routes requests, dispatches workers, and streams responses
- **Math MCP** exposes arithmetic, statistics, and trigonometry tools via the MCP protocol
- **Phoenix** receives OpenTelemetry traces so you can inspect every coordinator and worker step

## Troubleshooting

**`math-mcp` build fails**
The Dockerfile clones `EthanHenrickson/math-mcp` from GitHub. Ensure you have internet access during `docker compose up --build`.

**LibreChat shows "no models available"**
Aura may still be starting (it waits for math-mcp to be healthy first). Check `docker compose logs aura --tail 5` and refresh.

**Reset everything**

```bash
docker compose down -v
```
