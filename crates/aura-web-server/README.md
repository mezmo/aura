# Aura Web Server

> **Open Alpha** — Aura is under active development. APIs and configuration
> may change between releases. [Issues and feature requests](https://github.com/mezmo/aura/issues)
> are welcome — we'd love your feedback.

> **Part of the [Aura Project](../../README.md)** - A production-ready framework for building AI agents with declarative TOML configuration.

OpenAI-compatible web API server that exposes Aura agents through a standard chat completions endpoint.

## Features

- **OpenAI Compatible**: Implements `/v1/chat/completions` endpoint following OpenAI's API schema
- **Multi-Turn Conversations**: Maintains conversation context across requests
- **Full Tool Integration**: Supports all MCP transports (HTTP, SSE, STDIO) and filesystem tools
- **Health Monitoring**: `/health` endpoint for container health checks
- **Production Ready**: Stateless processing with pre-built agent, Docker-ready

## Quick Start

**For full setup instructions, see the [main README](../../README.md#setup).**

```bash
# Build the server
cargo build --release --bin aura-web-server

# Start with default config (config.toml)
cargo run --bin aura-web-server

# Or with custom configuration
CONFIG_PATH=my-config.toml cargo run --bin aura-web-server

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

### Chat Completions
```bash
POST /v1/chat/completions
Content-Type: application/json
```

Request body:
```json
{
  "model": "gpt-4o-mini",
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
  "usage": null
}
```

## Testing with curl

```bash
# Health check
curl -X GET http://127.0.0.1:8080/health

# Chat completion
curl -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [
      {"role": "user", "content": "What tools do you have available?"}
    ]
  }'
```

## Running Integration Tests

Integration tests verify streaming and tool execution functionality. Tests use a mock MCP server for tool execution and make LLM API calls to validate end-to-end behavior.

### Running Tests

**Automated** (recommended):
```bash
# Runs all tests with automatic setup/teardown
./crates/aura-web-server/tests/run_tests.sh
```

**Manual**:
```bash
# 1. Start mock MCP server (in one terminal)
python3 tests/integration/mock_mcp_server.py 9999

# 2. Start the server with test config (in another terminal)
CONFIG_PATH=crates/aura-web-server/tests/test-config.toml \
  cargo run --bin aura-web-server

# 3. Run tests (in a third terminal)
# All integration tests
cargo test --package aura-web-server --tests

# Or specific test suite
cargo test --package aura-web-server --test streaming_tests
```

### Prerequisites

- `OPENAI_API_KEY` environment variable (for LLM calls during tests)
- Python 3 (for mock MCP server)

## Configuration

The server uses the Aura TOML configuration system. See the [main README](../../README.md#configuration) for:
- LLM provider configuration (OpenAI, Anthropic)
- MCP server setup (HTTP, SSE, STDIO)
- Vector store and RAG integration
- Agent settings and prompts

Example configurations are in the [`examples/`](../../examples/) directory.

## Architecture

- **Pre-built Agent**: Agent is built once at startup from TOML config
- **Stateless Requests**: Each HTTP request is processed independently
- **Multi-Turn Support**: Conversation history passed via messages array
- **OpenAI Compatible**: Request/response schemas match OpenAI's format
- **Error Handling**: Proper HTTP status codes and error responses

## Deployment

**Docker**: See [DOCKER.md](../../DOCKER.md) for containerized deployment

**Environment Variables**:
- `CONFIG_PATH` - Path to configuration file (default: `config.toml`)
- `HOST` - Server bind address (default: `127.0.0.1`)
- `PORT` - Server port (default: `8080`)

## See Also

- [Main Documentation](../../README.md) - Full project setup and features
- [Docker Guide](../../DOCKER.md) - Container deployment