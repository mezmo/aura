# A2A Integration

This module wires the [A2A RC 1.0](https://github.com/a2a-protocol) protocol into the Aura web server via [`a2a-rs-server`](https://crates.io/crates/a2a-rs-server).

## Endpoints

| Method | Path | Transport | Description |
|--------|------|-----------|-------------|
| `GET` | `/.well-known/agent-card.json` | — | Agent card (capability discovery) |
| `GET` | `/.well-known/agent.json` | — | Agent card (v0.2 fallback alias) |
| `GET` | `/health` | — | Health check |
| `POST` | `/a2a/v1/message:send` | REST (HTTP+JSON) | Send a message, receive a task |
| `POST` | `/a2a/v1/message:stream` | REST SSE | Send a message, stream task updates as SSE (see [Streaming semantics](#streaming-semantics)) |
| `GET` | `/a2a/v1/tasks` | REST | List tasks |
| `GET` | `/a2a/v1/tasks/{id}` | REST | Get a task by ID |
| `POST` | `/a2a/v1/tasks/{id}:cancel` | REST | Cancel a task |
| `GET` | `/a2a/v1/tasks/{id}:subscribe` | REST SSE | Subscribe to updates for an in-flight task (see [Streaming semantics](#streaming-semantics)) |
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

## Streaming semantics

REST streaming (`message:stream`, `tasks/{id}:subscribe`) is served by local handlers in
`aura-web-server/src/a2a/overrides.rs`, **not** by the upstream `a2a-rs-server` 1.0.26 REST
router. The overrides exist to fix a race condition in the upstream crate.

### The race in upstream 1.0.26

Upstream `rest_send_streaming_message` (`a2a-rs-server-1.0.26/src/rest.rs:397`) and
`rest_subscribe_to_task` (line 613) both follow this ordering:

```text
task = handle_message(...);                 // worker spawns and may broadcast immediately
task_store.insert(task);
rx = state.subscribe_events();              // <-- TOO LATE
broadcast(StreamResponse::Task(task));
yield initial Task(task);
loop { rx.recv() ... }
```

Any event the handler emits before `subscribe_events()` is broadcast on a channel with no
subscribers — `tokio::sync::broadcast` drops those. The event is still persisted to the task
store, but it never reaches the SSE consumer.

For `AuraMessageHandler` (`a2a/handler.rs`), which `tokio::spawn`s a worker and returns
immediately, the race window is unbounded. The worker can complete and emit
`StatusUpdate(Completed)` before the upstream code reaches `subscribe_events()`; the client
then loops on `rx.recv()` forever waiting for a terminal event that already happened.

### What the overrides change

Both override handlers subscribe to the broadcast channel **first**, then read the snapshot
or invoke the handler:

```text
rx = event_tx.subscribe();                  // FIRST
task = handle_message(...);                 // (or get_flexible(id) for :subscribe)
yield initial Task(task);
loop { rx.recv() ... }
```

This collapses the race window to "may yield one duplicate event" — every A2A client de-dupes
on `task_id` / `artifact_id`, so duplicates are harmless. Lagged receivers (the broadcast
buffer overflowed) are surfaced as a `statusUpdate` event with a `lagged_events` metadata
counter; the upstream code silently swallows `Lagged`. If the post-lag task-store snapshot
shows a terminal state — i.e. the dropped events almost certainly included the terminal
`StatusUpdate` — the loop breaks rather than waiting forever on a channel that will never
deliver another event for this task.

The `StreamResponse::Message` matching path uses the `context_id` cached from the initial
task snapshot, not a non-blocking task-store fetch as upstream does. The upstream
`task_store.get(target).now_or_never()` returns `None` under any lock contention and silently
drops the Message; comparing against a cached string is race-free.

### JSON-RPC streaming — known limitation

The JSON-RPC methods `message/stream` and `tasks/resubscribe`
(`a2a-rs-server-1.0.26/src/server.rs:1052` and `:1376`) have the **same** race condition
upstream. They are **not** overridden here because the upstream JSON-RPC entry point is a
single handler that dispatches by `method` internally — intercepting it cleanly would mean
re-implementing every JSON-RPC method, which is too much surface area for a stopgap.

Until upstream ships a fix, clients that need reliable streaming should prefer the REST
endpoints (`:stream`, `:subscribe`). JSON-RPC streaming is best-effort and may hang on
short-lived tasks that complete during the subscribe window.

Upstream fix in flight: [tolgaki/a2a-rs#6](https://github.com/tolgaki/a2a-rs/pull/6) covers
all four endpoints (both REST and JSON-RPC) with the same `subscribe`-then-snapshot reorder.
When that merges and ships in a crates.io release, the local overrides in this directory can
be deleted and this caveat removed.

## Module layout

| File | Purpose |
|------|---------|
| `aura-web-server/src/a2a/mod.rs` | Module root; also defines `extract_auth_context`, the shared header-to-`AuthContext` mapping used by both the upstream `A2aServer` and the local override handlers |
| `aura-web-server/src/a2a/handler.rs` | `AuraMessageHandler` — implements the `MessageHandler` trait, bridges A2A messages to the Aura agent execution pipeline |
| `aura-web-server/src/a2a/overrides.rs` | Local REST SSE handlers for `:stream` and `:subscribe` that fix the upstream race (see [Streaming semantics](#streaming-semantics)) |
