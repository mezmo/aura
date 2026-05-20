# A2A Integration

This module wires the [A2A RC 1.0](https://github.com/a2a-protocol) protocol into the Aura web server via [`a2a-rs-server`](https://github.com/a2aproject/a2a-rs).

## Endpoints

| Method | Path | Transport | Description |
|--------|------|-----------|-------------|
| `GET` | `/.well-known/agent-card.json` | — | Agent card (capability discovery) |
| `GET` | `/health` | — | Health check |
| `POST` | `/a2a/v1/message:send` | REST (HTTP+JSON) | Send a message; returns task in `Working` state immediately |
| `POST` | `/a2a/v1/message:stream` | REST (SSE) | Send a message and stream task updates |
| `GET` | `/a2a/v1/tasks` | REST | List tasks |
| `GET` | `/a2a/v1/tasks/{id}` | REST | Get a task by ID |
| `POST` | `/a2a/v1/tasks/{id}:cancel` | REST | Cancel a task |
| `GET` | `/a2a/v1/tasks/{id}:subscribe` | REST (SSE) | Subscribe to task updates |
| `GET/POST` | `/a2a/v1/tasks/{id}/subscribe` | REST (SSE) | Subscribe to task updates (legacy path) |
| `POST` | `/a2a/v1/tasks/{id}/cancel` | REST | Cancel a task (legacy path) |
| `POST` | `/a2a/v1/rpc` | JSON-RPC 2.0 | All of the above via JSON-RPC envelope |

### `message:send` — immediate return

`AuraRequestHandler` forces `return_immediately = true` on every `message:send` request. The HTTP response returns as soon as the task is queued in `Working` state, without waiting for the agent to finish. Poll `GET /a2a/v1/tasks/{id}` or subscribe via `message:stream` / `tasks/{id}:subscribe` to track completion.

## Testing with curl

Assumes the server is running on `localhost:8080`.

### Agent card

```bash
curl http://localhost:8080/.well-known/agent-card.json | jq .
```

### Health check

```bash
curl http://localhost:8080/health
```

---

### REST — send a message

```bash
curl -s -X POST http://localhost:8080/a2a/v1/message:send \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{
    "message": {
      "messageId": "msg-001",
      "role": "ROLE_USER",
      "parts": [{ "text": "What is 2 + 2?" }]
    }
  }' | jq .
```

The response is a task object in `Working` state. Grab the `id` field for follow-up calls.

### REST — get a task by ID

```bash
curl -s http://localhost:8080/a2a/v1/tasks/<task-id> | jq .
```

### REST — list tasks

```bash
curl -s http://localhost:8080/a2a/v1/tasks | jq .
```

### REST — cancel a task

```bash
curl -s -X POST http://localhost:8080/a2a/v1/tasks/<task-id>:cancel | jq .
```

---

### JSON-RPC — send a message

```bash
curl -s -X POST http://localhost:8080/a2a/v1/rpc \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{
    "jsonrpc": "2.0",
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "msg-002",
        "role": "ROLE_USER",
        "parts": [{ "text": "Summarize the A2A protocol." }]
      }
    },
    "id": 1
  }' | jq .
```

### JSON-RPC — get a task

```bash
curl -s -X POST http://localhost:8080/a2a/v1/rpc \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{
    "jsonrpc": "2.0",
    "method": "GetTask",
    "params": { "id": "<task-id>" },
    "id": 2
  }' | jq .
```

### JSON-RPC — cancel a task

```bash
curl -s -X POST http://localhost:8080/a2a/v1/rpc \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{
    "jsonrpc": "2.0",
    "method": "CancelTask",
    "params": { "id": "<task-id>" },
    "id": 3
  }' | jq .
```

---

## Notes

- **`A2A-Version` is optional** — when present on `/a2a/v1/rpc` requests, the version is validated. An unsupported value returns `-32009 Version not supported`. REST endpoints do not enforce the header.
- **`messageId` and `role` are required** on the `Message` object — malformed bodies return `-32602 Invalid params`.
- **Text-only parts** — the executor only accepts `text` parts; `file` and `data` parts return an error.
- Tasks are stored in the `a2a-rs-server` in-memory `TaskStore` for the lifetime of the process. Use `GET /a2a/v1/tasks/{id}` or `GetTask` to poll after `message:send` returns.
- Request headers passed to `/a2a/v1/message:send` are forwarded to the agent's MCP connections (same `headers_from_request` mechanism as the OpenAI-compatible endpoint).

## Module layout

| File | Purpose |
|------|---------|
| `aura-web-server/src/a2a/mod.rs` | Module root — re-exports `AuraAgentExecutor` and `AuraRequestHandler` |
| `aura-web-server/src/a2a/agent_executor.rs` | `AuraAgentExecutor` — implements the `AgentExecutor` trait; bridges A2A task execution to the Aura agent streaming pipeline and builds the agent card |
| `aura-web-server/src/a2a/request_handler.rs` | `AuraRequestHandler` — wraps `DefaultRequestHandler` and forces `return_immediately = true` on every `message:send` so HTTP responses return immediately in `Working` state |
