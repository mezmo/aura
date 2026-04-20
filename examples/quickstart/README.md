# Aura Quickstart

Spin up a fully working AI agent stack in under a minute:

- **Aura** — the AI agent server
- **LibreChat** — a ChatGPT-style web UI connected to Aura
- **Phoenix** — an LLM trace viewer for inspecting every tool call, prompt, and token

## Setup

All commands below should be run from the quickstart directory:

```bash
cd examples/quickstart
```

### 1. Configure your LLM provider

```bash
cp .env.example .env
```

Edit `.env` and set your provider, model, and API key:

```bash
LLM_PROVIDER=openai          # or: anthropic, ollama
LLM_MODEL=gpt-5.2            # or: claude-sonnet-4-20250514, llama3.1
LLM_API_KEY=sk-...            # your API key (use "unused" for Ollama)
```

That's it — the config is wired up automatically. See `.env.example` for all provider examples.

### 2. Start everything

```bash
docker compose up -d
```

### 3. Open the UIs

| Service | URL | Description |
|---------|-----|-------------|
| LibreChat | <http://localhost:3080> | Chat with your agent |
| Phoenix | <http://localhost:6006> | Inspect LLM traces |
| Aura API | <http://localhost:3000> | OpenAI-compatible API |

**LibreChat first-time setup:** Create your user account on the signup page. The agent model is pre-configured as "Aura".

> **Tip:** Check startup progress with `docker compose logs -f aura`.

## Customize Your Agent

Edit `config.toml` to change agent behavior, add tools, or enable RAG.
Edit `.env` to switch LLM providers. Then apply changes:

```bash
docker compose up -d        # picks up .env changes and recreates if needed
```

> **Note:** `docker compose restart aura` is fine for `config.toml`-only changes, but
> `.env` changes require `docker compose up -d` to take effect.

### Switch LLM provider

Update `LLM_PROVIDER`, `LLM_MODEL`, and `LLM_API_KEY` in `.env`, then `docker compose up -d`.

For **Ollama**, also uncomment the `base_url` and `fallback_tool_parsing` lines in `config.toml` and set `LLM_BASE_URL` in `.env`.

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

Uncomment the `[[vector_stores]]` section in `config.toml`. Options:

- **Qdrant** (self-hosted): add a Qdrant instance to the compose file or point at an external one. Embeddings can be generated via OpenAI or AWS Bedrock.
- **AWS Bedrock Knowledge Base** (managed): set `type = "bedrock_kb"` with a `knowledge_base_id` and `region`. No embedding model needed — the KB manages embeddings internally.

See [`examples/reference.toml`](../reference.toml) for both.

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
