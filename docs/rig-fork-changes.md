# Rig Fork Modifications

## Repository

- **Fork**: https://github.com/mezmo/rig
- **Branch**: `fix/toolserver-span-propagation`
- **Base**: rig-core 0.28.x

## Why a Fork?

Aura depends on a small set of fixes to rig-core that have not yet been accepted upstream. The fork is a superset of upstream and is intended to be temporary.

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

## Usage

```toml
# Cargo.toml
[workspace.dependencies]
rig-core = { git = "https://github.com/mezmo/rig.git", branch = "fix/toolserver-span-propagation", features = ["rmcp"] }

[patch.crates-io]
rig-core = { git = "https://github.com/mezmo/rig.git", branch = "fix/toolserver-span-propagation" }
```

## Removing the Fork

When upstream rig-core incorporates these fixes:

1. Update `rig-core` to the official release version
2. Remove the `[patch.crates-io]` section
3. Delete this document
