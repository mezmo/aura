# A2A Integration

This module wires the [A2A RC 1.0](https://github.com/a2a-protocol) protocol into the Aura web server via [`a2a-rs-server`](https://crates.io/crates/a2a-rs-server).

## Endpoints

| Method | Path | Transport | Description |
|--------|------|-----------|-------------|
| `GET` | `/.well-known/agent-card.json` | — | Agent card (capability discovery) |
| `GET` | `/.well-known/agent.json` | — | Agent card (v0.2 fallback alias) |
| `GET` | `/health` | — | Health check |
| `POST` | `/a2a/v1/message:send` | REST (HTTP+JSON) | Send a message, receive a task |
| `GET` | `/a2a/v1/tasks` | REST | List tasks |
| `GET` | `/a2a/v1/tasks/{id}` | REST | Get a task by ID |
| `POST` | `/a2a/v1/tasks/{id}:cancel` | REST | Cancel a task |
| `POST` | `/a2a/v1/rpc` | JSON-RPC 2.0 | All of the above via JSON-RPC envelope |

The `A2A-Version` header is **optional**. When present on a JSON-RPC request to `/a2a/v1/rpc`, the version is validated and an unsupported value returns `-32009 Version not supported`. REST endpoints do not enforce the header.

### A2A Auth Header Extraction

When the A2A endpoint is enabled, these variables map incoming request headers to the `AuthContext` passed to agent tasks:

| Variable | Purpose |
|---|---|
| `A2A_HEADER_USER_ID` | Name of the request header whose value becomes `AuthContext.user_id` |
| `A2A_HEADER_AUTHORIZATION` | Name of the request header whose value becomes `AuthContext.access_token` |
| `A2A_HEADER_AUTHORIZATION_STRIP_PREFIX` | Optional prefix to strip from the authorization header value (e.g. `Bearer `) |

All three are optional. If unset, `user_id` defaults to `""` and `access_token` defaults to `""`.

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

The response is a task object. Grab the `id` field for follow-up calls.

### REST — get a task by ID

```bash
curl -s http://localhost:8080/a2a/v1/tasks/<task-id> | jq .
```

### REST — list tasks

```bash
curl -s http://localhost:8080/a2a/v1/tasks | jq .
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
        "parts": [{ "text": "Summarise the A2A protocol." }]
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
- Tasks are stored in the `a2a-rs-server` in-memory `TaskStore` for the lifetime of the process. Use `GET /a2a/v1/tasks/{id}` or `GetTask` to poll after `message:send` returns.
- The agent card is served at both `/.well-known/agent-card.json` (RC 1.0) and `/.well-known/agent.json` (v0.2 fallback) for interoperability.

## Module layout

| File | Purpose |
|------|---------|
| `aura-web-server/src/a2a/mod.rs` | Module root |
| `aura-web-server/src/a2a/handler.rs` | `AuraMessageHandler` — implements the `MessageHandler` trait, bridges A2A messages to the Aura agent execution pipeline |
