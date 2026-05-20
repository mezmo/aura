# AURA Web Server

> **Open Alpha** — AURA is under active development. APIs and configuration
> may change between releases. [Issues and feature requests](https://github.com/mezmo/aura/issues)
> are welcome — we'd love your feedback.

> **Part of the [AURA Project](../../README.md)** - A production-ready framework for building AI agents with declarative TOML configuration.

OpenAI-compatible web API server that exposes AURA agents through a standard chat completions endpoint.

## Features

- **OpenAI Compatible**: Implements `/v1/chat/completions` endpoint following OpenAI's API schema
- **Multi-Turn Conversations**: Maintains conversation context across requests
- **Full Tool Integration**: Supports all MCP transports (HTTP, SSE, STDIO) and client-side tool passthrough
- **Health Monitoring**: `/health` endpoint for container health checks
- **Production Ready**: Stateless processing with pre-built agent, Docker-ready

## Quick Start

**For full setup instructions, see the [main README](../../README.md#setup).**

```bash
# Build the server
cargo build --release --bin aura-web-server

# Start with default config (config.toml)
cargo run --bin aura-web-server

# Or with custom configuration file
CONFIG_PATH=my-config.toml cargo run --bin aura-web-server

# Or with a directory of configs (serves multiple agents)
CONFIG_PATH=configs/ cargo run --bin aura-web-server

# Custom host/port
HOST=0.0.0.0 PORT=3000 cargo run --bin aura-web-server
```

## API Endpoints

### Health Check
```bash
GET /health
```
Response:
```json
{"status": "healthy"}
```

### List Models (Agents)
```bash
GET /v1/models
```
Returns all loaded agents. Each agent's `alias` (or `name` if no alias is set) is its model `id`. The `owned_by` field defaults to the underlying LLM provider (e.g. `"openai"`, `"anthropic"`) and can be overridden with `model_owner` in the agent config. Clients like LibreChat and OpenWebUI use this endpoint to populate their model picker.

Response:
```json
{
  "object": "list",
  "data": [
    {"id": "devops", "object": "model", "created": 1677649963, "owned_by": "mezmo"},
    {"id": "research-assistant", "object": "model", "created": 1677649963, "owned_by": "mezmo"}
  ]
}
```

### Chat Completions
```bash
POST /v1/chat/completions
Content-Type: application/json
```

The `model` field selects which agent handles the request by matching against agent `alias` or `name`. Agent selection follows this order:
1. If only one config is loaded, it is always used (the `model` field is ignored)
2. Otherwise, `model` is matched first, then `DEFAULT_AGENT` if `model` is absent
3. Returns a 400 error if multiple configs are loaded and neither `model` nor `DEFAULT_AGENT` resolves to a match

Request body:
```json
{
  "model": "devops",
  "messages": [
    {"role": "user", "content": "What tools do you have available?"}
  ]
}
```

Response:
```json
{
  "id": "chatcmpl-1865d39015e49520",
  "object": "chat.completion",
  "created": 1758043845,
  "model": "openai/gpt-4o-mini",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "I have the following tools available:\n\n1. **Log Analysis**: Export logs, analyze for root causes, and apply time-based filtering.\n2. **Knowledge Base**: Search AWS Bedrock knowledge bases for documentation and procedures.\n3. **Current Time**: Get the current timestamp for time-based operations.\n4. **Pipeline Management**: List and analyze Mezmo pipelines.\n5. **Filesystem**: Read configuration files and logs when needed."
    },
    "finish_reason": "stop"
  }],
  "usage": {
    "prompt_tokens": 42,
    "completion_tokens": 118,
    "total_tokens": 160
  }
}
```

## Testing with curl

```bash
# Health check
curl -X GET http://127.0.0.1:8080/health

# List available agents
curl http://127.0.0.1:8080/v1/models

# Chat completion (uses DEFAULT_AGENT when model is omitted)
curl -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "messages": [
      {"role": "user", "content": "What tools do you have available?"}
    ]
  }'

# Chat completion with a specific agent
curl -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "devops",
    "messages": [
      {"role": "user", "content": "What tools do you have available?"}
    ]
  }'
```

## Running Integration Tests

Integration tests verify streaming and tool execution functionality. Tests use a mock MCP server for tool execution and make LLM API calls to validate end-to-end behavior.

### Running Tests

```bash
# 1. Start the server with test config (in one terminal)
CONFIG_PATH=crates/aura-web-server/tests/test-config.toml \
  cargo run --bin aura-web-server

# 2. Run tests (in another terminal)
# All integration tests
cargo test --package aura-web-server --tests

# Or a specific test suite
cargo test --package aura-web-server --test streaming_tests
```

### Prerequisites

- `OPENAI_API_KEY` environment variable (for LLM calls during tests)

## Configuration

The server uses the AURA TOML configuration system. See the [main README](../../README.md#configuration) for:
- LLM provider configuration (OpenAI, Anthropic)
- MCP server setup (HTTP, SSE, STDIO)
- Vector store and RAG integration
- Agent settings and prompts

Example configurations are in the [`examples/`](../../examples/) directory.

## Architecture

- **Multi-Agent Serving**: Load multiple agents from a config directory, selectable via the `model` field
- **Stateless Requests**: Each HTTP request is processed independently
- **Multi-Turn Support**: Conversation history passed via messages array
- **OpenAI Compatible**: Request/response schemas match OpenAI's format
- **Error Handling**: Proper HTTP status codes and error responses

## Deployment

**Environment Variables**:

| Variable | Default | Description |
|---|---|---|
| `CONFIG_PATH` | `config.toml` | Path to a config file or directory of configs |
| `HOST` | `127.0.0.1` | Server bind address |
| `PORT` | `8080` | Server port |
| `DEFAULT_AGENT` | *(none)* | Agent name or alias used when `model` is omitted. Not needed when only one config is loaded. |
| `AURA_CUSTOM_EVENTS` | `false` | Emit `aura.*` SSE events alongside OpenAI-compatible chunks |
| `AURA_EMIT_REASONING` | `false` | Emit `aura.reasoning` events (requires `AURA_CUSTOM_EVENTS=true`) |
| `TOOL_RESULT_MODE` | `none` | How tool results are streamed: `none`, `open-web-ui`, or `aura` |
| `TOOL_RESULT_MAX_LENGTH` | `1000` | Truncation limit for streamed tool results (0 = no truncation) |
| `STREAMING_TIMEOUT_SECS` | `900` | Max duration for a streaming request before cancellation |
| `FIRST_CHUNK_TIMEOUT_SECS` | `90` | Max wait for the first LLM chunk before treating the connection as hung (0 = disabled) |
| `SHUTDOWN_TIMEOUT_SECS` | `30` | Grace period for in-flight streams after SIGTERM/SIGINT |
| `STREAMING_BUFFER_SIZE` | `400` | SSE chunk buffer size; higher values reduce latency but increase memory use |

## See Also

- [Main Documentation](../../README.md) - Full project setup and features
