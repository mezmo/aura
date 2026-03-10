# Aura Quickstart

One command to spin up a fully working AI agent stack:

- **Aura** — the AI agent server
- **LibreChat** — a ChatGPT-style web UI connected to Aura
- **Phoenix** — an LLM trace viewer for inspecting every tool call, prompt, and token

## Setup

### 1. Add your API key

```bash
cp .env.example .env
```

Edit `.env` and paste your OpenAI (or Anthropic) API key.

### 2. Start everything

```bash
docker compose up
```

### 3. Open the UIs

| Service | URL | Description |
|---------|-----|-------------|
| LibreChat | <http://localhost:3080> | Chat with your agent |
| Phoenix | <http://localhost:6006> | Inspect LLM traces |
| Aura API | <http://localhost:3000> | OpenAI-compatible API |

**LibreChat first-time setup:** Create your user account on the signup page. The agent model is pre-configured as "Aura".

## Customize Your Agent

Edit `config.toml` in this directory, then restart:

```bash
docker compose restart aura
```

### Switch LLM provider

Uncomment the Anthropic or Ollama block in `config.toml` and set the matching API key in `.env`.

### Add MCP tool servers

Uncomment the `[mcp]` section in `config.toml` and point it at your MCP server:

```toml
[mcp]
sanitize_schemas = true

[mcp.servers.my_tools]
transport = "http_streamable"
url = "http://host.docker.internal:9000/mcp"
```

Use `host.docker.internal` to reach services running on your host machine.

### Add RAG (vector search)

Uncomment the `[[vector_stores]]` section in `config.toml`. You'll need a Qdrant instance — you can add one to the compose file or point at an external one.

### Serve multiple agents

Create a `configs/` directory with one TOML file per agent, update the `CONFIG_PATH` in `docker-compose.yml` to point to the directory, and mount it. Clients which support picking models will see each agent in their model picker via `GET /v1/models`.

### Full configuration reference

See [`examples/reference.toml`](../reference.toml) for all available options.

## Architecture

```
┌───────────────────────────────────────────────────┐
│  docker compose                                   │
│                                                   │
│  ┌────────────┐     ┌──────────┐    ┌──────────┐  │
│  │ LibreChat  │────▶│   Aura   │───▶│ Phoenix  │  │
│  │   :3080    │     │  :3030   │    │  :6006   │  │
│  └─────┬──────┘     └────┬─────┘    └──────────┘  │
│        │                 │          OTel traces   │
│  ┌─────┴──────┐          │                        │
│  │  MongoDB   │          │                        │
│  │ (LibreChat │          │                        │
│  │  storage)  │          │                        │
│  └────────────┘          │                        │
└──────────────────────────┼────────────────────────┘
         ▲                 │
         │                 ▼
      Browser         LLM Provider
                     (OpenAI, etc.)
```

- **LibreChat** sends chat requests to Aura's OpenAI-compatible `/v1/chat/completions` endpoint. MongoDB is used by LibreChat internally for user accounts and conversation history — Aura does not use it.
- **Aura** calls the configured LLM provider, executes MCP tools, and streams responses back
- **Phoenix** receives OpenTelemetry traces from Aura so you can inspect every step

## Troubleshooting

**LibreChat shows "no models available"**
Aura may still be starting. Wait for the health check to pass (`docker compose logs aura --tail 5`) and refresh.

**"connection refused" in Aura logs**
If referencing services on your host, use `host.docker.internal` instead of `localhost` in `config.toml`.

**Reset everything**

```bash
docker compose down -v
```
