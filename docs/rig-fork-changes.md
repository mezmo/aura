# Rig Fork Modifications

## Repository

- **Fork**: https://github.com/mezmo/rig
- **Branch**: `mezmo`
- **Base**: rig-core 0.28.x

## Why a Fork?

Aura relies on a small set of enhancements to rig-core required for its streaming architecture. These are maintained in this fork; as Rig evolves, the goal is to converge with upstream or adapt as needed.

## Changes

### 1. StreamingPromptHook history ordering fix

In Rig's multi-turn streaming mode, the `StreamingPromptHook` received chat history in the wrong order, causing hooks to see stale context. The fix corrects the ordering so hooks always receive the full conversation history at the point of invocation.

### 2. ToolCall and ToolResult event emission

Upstream Rig executes tools during multi-turn streaming but does not yield the events to the stream consumer. The fork adds:

- **ToolCall emission**: yields `StreamedAssistantContent::ToolCall` before tool execution so clients can observe tool calls in real-time.
- **ToolResult emission**: adds a `ToolResult` variant to `StreamedAssistantContent` and yields it after execution, making actual results visible to stream consumers.

This is required for OpenAI-compatible SSE streaming with tool observability.

### 3. OpenTelemetry span propagation

Adds `tracing::Instrument` spans around tool server calls so that OpenTelemetry traces propagate correctly through the tool execution path.

### 4. Content-Type header fix

Ensures the correct `Content-Type` header is set on provider API requests.

## Tool Execution Order

Aura's streaming event correlation (tool_call_id tracking between hook and MCP execution) relies on Rig's streaming mode executing tools **sequentially**:

```
tool_call_1 → execute_1 → result_1 → tool_call_2 → execute_2 → result_2 → ...
```

This is guaranteed by the `.await` on each tool's async block in `rig-core/src/agent/prompt_request/streaming.rs` — each tool completes before the next stream item is processed.

**If upgrading Rig**, verify this sequential guarantee still holds. Look for `FuturesUnordered` or parallel execution patterns in the streaming handler — those would break the FIFO correlation logic used for Aura's streaming event tracking.

## Usage

```toml
# Cargo.toml
[workspace.dependencies]
rig-core = { git = "https://github.com/mezmo/rig.git", branch = "mezmo", features = ["rmcp"] }

[patch.crates-io]
rig-core = { git = "https://github.com/mezmo/rig.git", branch = "mezmo" }
```

## Removing the Fork

If upstream rig-core incorporates equivalent functionality:

1. Update `rig-core` to the official release version
2. Remove the `[patch.crates-io]` section
3. Update this document
