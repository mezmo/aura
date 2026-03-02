# Request Lifecycle Architecture

## Overview

Aura manages per-request state (cancellation tokens, subscriptions) across streaming SSE connections. This document covers the lifecycle, timeout configuration, and known limitations.

---

## Request Flow

```
Client POST /v1/chat/completions
         │
         ▼
┌─────────────────────────────────────┐
│ Shutdown middleware check           │
│   (503 if shutdown_token cancelled) │
└─────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│ Generate request_id (UUID)          │
└─────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│ Spawn producer task                 │
│   - Register cancellation token     │
│   - Subscribe to progress events    │
│   - Subscribe to tool events        │
└─────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│ Stream chat with TimeoutHook        │
│   - Tool calls set thread context   │
│   - Tool results clear context      │
└─────────────────────────────────────┘
         │
         ├──────────────────┬──────────────────┐
         ▼                  ▼                  ▼
┌────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│ Normal         │  │ Timeout/         │  │ Shutdown         │
│ completion     │  │ Disconnect       │  │ (grace expired)  │
└────────────────┘  └──────────────────┘  └──────────────────┘
         │                  │                  │
         │                  ▼                  ▼
         │          ┌──────────────────┐  ┌──────────────────┐
         │          │ Cancel token     │  │ Cancel hook +    │
         │          │ Send MCP cancel  │  │  registry        │
         │          │ Evict from pool  │  │ Send [DONE]      │
         │          └──────────────────┘  │ Then MCP cleanup │
         │                  │             │ (no pool evict)  │
         │                  │             └──────────────────┘
         └──────────┬───────┴──────────────────┘
                    ▼
┌─────────────────────────────────────┐
│ Cleanup (RAII guard)                │
│   - Unregister cancellation         │
│   - Unsubscribe progress            │
│   - Unsubscribe tool events         │
└─────────────────────────────────────┘
```

---

## Timeout Configuration

### Production Defaults

| Setting | Default | Env Variable | Purpose |
|---------|---------|--------------|---------|
| Stream timeout | 15 min | `STREAMING_TIMEOUT_SECS` | Max request duration |
| Shutdown grace period | 30 sec | `SHUTDOWN_TIMEOUT_SECS` | Time for in-flight requests to finish on shutdown |
| Heartbeat | 15 sec | — | Disconnect detection |

### Rationale

- **Stream timeout (15 min)**: Supports long-running MCP tools. Set to 0 to disable (not recommended).
- **Heartbeat (15 sec)**: Standard SSE keepalive. Detects disconnect during silent tool execution.

---

## Tool Event Correlation via FIFO Queue

### The Challenge

Rig spawns tool execution in separate tokio tasks. The hook context (where LLM decides to call a tool) and the execution context (where MCP actually runs) are decoupled, requiring a mechanism to correlate `tool_call_id` across these boundaries.

### Current Approach

FIFO queue per request in `tool_event_broker.rs`:

1. **Hook fires `on_tool_call`** → Push `tool_call_id` to request's `VecDeque`
2. **MCP execution starts** → Peek queue to get `tool_call_id` and `progress_token`
3. **Hook fires `on_tool_result`** → Pop `tool_call_id` from queue (cleanup)

```rust
// tool_event_broker.rs
pending_tool_calls: RwLock<HashMap<String, VecDeque<String>>>
// Hook pushes when on_tool_call fires
// Execution peeks when tool starts
// Hook pops when on_tool_result fires
```

### Sequential Execution Guarantee

This design relies on **Rig 0.28 streaming mode executing tools sequentially**. The FIFO ordering is guaranteed because each tool completes before the next begins.

**Critical**: If upgrading Rig, verify sequential execution is preserved. See `docs/rig-tool-execution-order.md` for validation methodology.

---

## Cleanup Mechanism

`RequestResourceGuard` (RAII) ensures cleanup runs even on panic:

```rust
// Drop impl spawns async cleanup:
agent.clear_mcp_request_id().await;
RequestCancellation::unregister(&request_id);
request_progress_unsubscribe(&request_id).await;
tool_event_unsubscribe(&request_id).await;
tool_usage_unsubscribe(&request_id).await;
```

The guard uses `tokio::runtime::Handle::try_current()` to avoid panics during runtime shutdown. If the runtime is already gone (process exit), cleanup is skipped — resources are reclaimed with the process.

---

## MCP Cancellation

On client disconnect or timeout:

1. `CancellationToken::cancel()` signals all waiting code
2. `agent.cancel_and_close_mcp()` sends `notifications/cancelled` to MCP servers
3. Agent evicted from pool to prevent stale connections

MCP servers receive the cancellation notification and can abort in-progress operations.

---

## Graceful Shutdown

The server uses a two-phase shutdown with separate cancellation tokens:

| Token | Cancelled | Purpose |
|-------|-----------|---------|
| `shutdown_token` | Immediately on signal | Middleware rejects new requests with 503 |
| `stream_shutdown_token` | After `SHUTDOWN_TIMEOUT_SECS` grace period | Terminates remaining in-flight streams |

### Shutdown Sequence

1. **SIGTERM/SIGINT** received
2. **Phase 1 (immediate)**: `shutdown_token` cancelled — middleware returns 503 for all new requests
3. **Grace period**: In-flight streams continue running for up to `SHUTDOWN_TIMEOUT_SECS` (default 30s). Streams that complete naturally during this window are unaffected.
4. **Phase 2 (drain)**: `stream_shutdown_token` cancelled — remaining streams:
   - Cancel hook + request registry (stops in-flight MCP tool execution)
   - Send `[DONE]` to client (before MCP cleanup, so client gets clean termination)
   - Run `cancel_and_close_mcp()` (send `notifications/cancelled`, close connections)
   - No pool eviction (pool is dying with the server)
5. **Actix stops**: Workers have 10s to complete Phase 2 cleanup

### Shutdown vs Disconnect/Timeout

| Behavior | Disconnect/Timeout | Shutdown |
|----------|-------------------|----------|
| Pool eviction | Yes | No (pool is dying) |
| `[DONE]` timing | After MCP cleanup (Timeout) / skipped (Disconnect) | Before MCP cleanup |
| Grace period | None — immediate cancel | Configurable (`SHUTDOWN_TIMEOUT_SECS`) |

---

## Test Timeouts

Tests use centralized constants in `tests/common/mod.rs`:

| Constant | Value | Purpose |
|----------|-------|---------|
| `HTTP_REQUEST` | 60s | HTTP request timeout (accounts for LLM latency) |
| `TOOL_START` | 30s | Wait for LLM to call tool |
| `POST_DISCONNECT_WAIT` | 5s | Verify cleanup after disconnect |
| `POLL_INTERVAL` | 100ms | File existence polling |

Per-test timeouts range from 30s (basic streaming) to 120s (header forwarding with tool execution).
