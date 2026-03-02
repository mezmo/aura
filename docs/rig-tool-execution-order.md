# Rig Tool Execution Order Research

**Date**: 2026-01-14
**Rig Version**: rig-core 0.28.0 (mezmo fork)
**Purpose**: Validate assumptions for FIFO queue design in tool_call_id correlation

---

## Summary

Rig handles tool execution differently depending on the mode:

| Mode | Execution | Order |
|------|-----------|-------|
| **Streaming** | Sequential | Deterministic (FIFO) |
| **Non-streaming** | Parallel | Deterministic (preserves order) |

**Aura uses streaming mode exclusively**, so tools execute sequentially. This validates using a simple FIFO queue for tool_call_id correlation.

---

## Streaming Mode Analysis (What Aura Uses)

In Rig's streaming implementation (`rig-core/src/agent/prompt_request/streaming.rs`):

### The Sequential Pattern

```rust
// Yield the tool_call event to stream
yield Ok(MultiTurnStreamItem::stream_item(
    StreamedAssistantContent::ToolCall(tool_call.clone())
));

// Execute the tool in an async block
let tc_result = async {
    let tool_span = tracing::Span::current();
    let tool_args = json_utils::value_to_json_string(&tool_call.function.arguments);

    // Hook fires BEFORE execution
    if let Some(ref hook) = self.hook {
        hook.on_tool_call(&tool_call.function.name, tool_call.call_id.clone(),
                          &tool_args, cancel_signal.clone()).await;
    }

    // ACTUAL TOOL EXECUTION
    let tool_result = match agent.tool_server_handle
        .call_tool(&tool_call.function.name, &tool_args).await {
        Ok(thing) => thing,
        Err(e) => e.to_string(),
    };

    // Hook fires AFTER execution
    if let Some(ref hook) = self.hook {
        hook.on_tool_result(&tool_call.function.name, tool_call.call_id.clone(),
                            &tool_args, &tool_result.to_string(),
                            cancel_signal.clone()).await;
    }

    // ... collect results ...
    Ok(tool_result)
}.instrument(tool_span).await;  // <-- The .await makes it SEQUENTIAL

// Yield the tool_result event
yield Ok(MultiTurnStreamItem::StreamUserItem(
    StreamedUserContent::ToolResult(tr)
));
```

### Key Observation

The `.await` on the async block ensures **each tool completes before the next stream item is processed**. This creates a strict sequential flow:

```
tool_call_1 → execute_1 → tool_result_1 → tool_call_2 → execute_2 → tool_result_2 → ...
```

---

## Non-Streaming Mode Analysis (For Reference)

In Rig's non-streaming implementation (`rig-core/src/agent/prompt_request/mod.rs`):

```rust
let tool_content = stream::iter(tool_calls)
    .then(|choice| async move {
        // ... execute tool ...
        agent.tools.call(tool_name, args.clone()).await?
    })
    .collect::<Vec<Result<UserContent, ToolSetError>>>()
    .await  // <-- Executes all futures CONCURRENTLY
```

Uses `stream::iter().then().collect().await` which:
- Creates futures for each tool call
- Executes them **concurrently** (not sequentially)
- Collects results in **original order** (deterministic)

---

## Design Implications for Aura

### Why FIFO Queue Works

Since streaming mode is sequential:

1. **Hook fires first**: `on_tool_call` pushes `tool_call_id` to queue
2. **Tool executes**: `call_tool_tracked` pops `tool_call_id` from queue
3. **Order guaranteed**: FIFO matches execution order

```
Hook: push(call_1) → push(call_2) → push(call_3)
Exec: pop() → call_1, pop() → call_2, pop() → call_3
```

### No Need for Complex Keys

Previous design used `(request_id, tool_name, args_hash)` as lookup key. This was:
- Complex to implement (canonical JSON serialization)
- Brittle (args format matching)
- Unnecessary given sequential execution

New design uses simple `(request_id)` → `VecDeque<tool_call_id>`:
- Simple push/pop operations
- No serialization concerns
- Relies on sequential execution guarantee

---

## Version Note

This analysis was verified against **rig-core 0.28.0** from the mezmo fork. If Rig's execution model changes in future versions, this assumption should be re-validated by reviewing the streaming implementation for sequential vs parallel tool execution patterns.
