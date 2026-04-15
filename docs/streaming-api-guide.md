# Streaming API Guide

OpenAI-compatible Server-Sent Events (SSE) streaming for real-time responses.

## Quick Start

```bash
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages": [{"role": "user", "content": "Hello!"}], "stream": true}'
```

## Configuration

### Tool Result Modes

The server supports three streaming modes, configured via CLI or environment variable:

| Mode | Tool Call Args | Tool Results | Use Case |
|------|----------------|--------------|----------|
| `none` (default) | Actual JSON | Not streamed | Spec-compliant API clients |
| `open-web-ui` | Empty `""` | Streamed via tool_calls | OpenWebUI "View Results" support |
| `aura` | Actual JSON | Via `aura.tool_complete` events | Custom clients with Aura events |

```bash
# Spec-compliant mode (default)
cargo run --bin aura-web-server

# OpenWebUI compatibility mode
cargo run --bin aura-web-server -- --tool-result-mode open-web-ui

# Aura events mode (requires AURA_CUSTOM_EVENTS=true)
AURA_CUSTOM_EVENTS=true cargo run --bin aura-web-server -- --tool-result-mode aura

# Via environment variable
TOOL_RESULT_MODE=aura AURA_CUSTOM_EVENTS=true cargo run --bin aura-web-server
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `TOOL_RESULT_MODE` | `none` | `none`, `open-web-ui`, or `aura` |
| `TOOL_RESULT_MAX_LENGTH` | `100` | Max chars for tool results (0 = no truncation) |
| `STREAMING_TIMEOUT_SECS` | `900` | Request timeout in seconds (0 = no timeout) |
| `STREAMING_BUFFER_SIZE` | `400` | Chunks to buffer before backpressure |
| `AURA_CUSTOM_EVENTS` | `false` | Enable custom `aura.*` events |
| `AURA_EMIT_REASONING` | `false` | Enable `aura.reasoning` events |
| `SHUTDOWN_TIMEOUT_SECS` | `30` | Grace period (seconds) for in-flight streams on shutdown |

## Custom Aura Events (Optional)

For enhanced client UX, you can enable custom Aura events that provide real-time tool status and timing:

```bash
AURA_CUSTOM_EVENTS=true cargo run --bin aura-web-server
```

### Custom Event Types

| Event | Description | Status |
|-------|-------------|--------|
| `aura.tool_requested` | LLM decided to call a tool (immediate UI feedback, has arguments) | ✅ Implemented |
| `aura.tool_start` | MCP execution actually begins (has progress_token for correlation) | ✅ Implemented |
| `aura.tool_complete` | Tool execution finished (with duration_ms, result/error) | ✅ Implemented |
| `aura.reasoning` | LLM reasoning content (requires `AURA_EMIT_REASONING=true`) | ✅ Implemented |
| `aura.progress` | MCP progress notifications during long-running tools | ✅ Implemented |
| `aura.session_info` | Session metadata (model, context window) emitted at stream start | ✅ Implemented |
| `aura.orchestrator.*` | Orchestration lifecycle events (see [Orchestration Events](#orchestration-events) below) | ✅ Implemented |

### Event Flow

```
LLM decides to call tool
       ↓
aura.tool_requested  ← Immediate UI feedback (tool_id, tool_name, arguments)
       ↓
MCP execution begins
       ↓
aura.tool_start      ← Has progress_token for correlation
       ↓
aura.progress        ← MCP server sends updates (uses progress_token)
       ↓
MCP execution ends
       ↓
aura.tool_complete   ← Final result (duration_ms, success, result/error)
```

### Event Formats

Custom events use the SSE `event:` field to distinguish from standard OpenAI chunks:

**Tool requested** (immediate UI feedback when LLM decides to call a tool):
```
event: aura.tool_requested
data: {"tool_id":"call_abc123","tool_name":"list_files","arguments":{"path":"/tmp"},"agent_id":"main","session_id":"sess_xyz"}
```

**Tool start** (when MCP execution actually begins):
```
event: aura.tool_start
data: {"tool_id":"call_abc123","tool_name":"list_files","progress_token":42,"agent_id":"main","session_id":"sess_xyz"}
```

Note: `progress_token` is included when available from the MCP client. Use it to correlate with `aura.progress` events.

**Tool complete (success)**:
```
event: aura.tool_complete
data: {"tool_id":"call_abc123","tool_name":"list_files","duration_ms":1234,"success":true,"result":"file1.txt\nfile2.txt... [truncated]","agent_id":"main","session_id":"sess_xyz"}
```

**Tool complete (failure)**:
```
event: aura.tool_complete
data: {"tool_id":"call_abc123","tool_name":"failing_tool","duration_ms":50,"success":false,"error":"Tool returned an error: Connection refused","agent_id":"main","session_id":"sess_xyz"}
```

Note:
- Successful tool results include the `result` field (truncated per `TOOL_RESULT_MAX_LENGTH`, default 100 chars)
- Tool errors are automatically detected from Rig's error format prefixes (`ToolCallError:`, `JsonError:`, `Tool returned an error:`)
- When detected, `success` is set to `false` and the `error` field contains the full error message

**Reasoning** (requires both flags):
```bash
AURA_CUSTOM_EVENTS=true AURA_EMIT_REASONING=true cargo run --bin aura-web-server
```
```
event: aura.reasoning
data: {"content":"Let me analyze the request...","agent_id":"main","session_id":"sess_xyz"}
```

**Progress** (MCP notifications from long-running tools):
```
event: aura.progress
data: {"message":"Processing step 3 of 5","phase":"mcp_progress","percent":60,"progress_token":42,"agent_id":"main","session_id":"sess_xyz"}
```

Note: Progress events are only emitted when:
1. `AURA_CUSTOM_EVENTS=true` is set
2. The MCP server sends `notifications/progress` messages during tool execution

**Session info** (emitted once at stream start):
```
event: aura.session_info
data: {"model":"gpt-5.2","model_context_limit":200000,"agent_id":"main","session_id":"sess_xyz"}
```

Note: `model_context_limit` comes from the `context_window` field in the `[agent]` TOML config section. If `context_window` is not set, `model_context_limit` is omitted from the event.

### Client Handling

Standard OpenAI clients will ignore these events (they only process `data:` lines without `event:` prefix). Custom clients can filter by event type:

```javascript
for (const line of chunk.split('\n')) {
  if (line.startsWith('event: ')) {
    const eventType = line.slice(7);
    // Handle aura.tool_start, aura.tool_complete, etc.
  }
  if (line.startsWith('data: ')) {
    const data = JSON.parse(line.slice(6));
    // Handle OpenAI chunk or custom event data
  }
}
```

### Correlation Fields

All custom events include correlation fields for tracing:

| Field | Description |
|-------|-------------|
| `session_id` | Chat session ID (from request metadata) |
| `trace_id` | OTEL trace ID (when available) |
| `agent_id` | Agent identifier (`main` for single-agent) |

#### Tool Event Correlation

Use these fields to correlate tool-related events:

| Correlation | Events | Field |
|-------------|--------|-------|
| Tool lifecycle | `tool_requested` → `tool_start` → `tool_complete` | `tool_id` |
| Progress updates | `tool_start` → `progress` | `progress_token` |

Example correlation:
```
tool_requested (tool_id: "call_abc") → shows arguments to user
      ↓
tool_start (tool_id: "call_abc", progress_token: 42) → MCP execution begun
      ↓
progress (progress_token: 42, progress: 50, total: 100) → correlates via token
      ↓
tool_complete (tool_id: "call_abc", duration_ms: 1234) → final result
```

## Orchestration Events

When `orchestration.enabled = true` and `AURA_CUSTOM_EVENTS=true`, the server emits orchestration-specific events covering the Plan/Execute/Synthesize/Evaluate lifecycle. These events are emitted alongside the standard `aura.*` events above.

### Orchestration Event Types

| Event | Description |
|-------|-------------|
| `aura.orchestrator.plan_created` | Coordinator decomposed query into a task DAG |
| `aura.orchestrator.direct_answer` | Coordinator answered without orchestration |
| `aura.orchestrator.clarification_needed` | Coordinator needs user clarification |
| `aura.orchestrator.task_started` | Worker began executing a task |
| `aura.orchestrator.task_completed` | Worker finished task (success/failure with duration) |
| `aura.orchestrator.worker_reasoning` | Worker reasoning content with task/worker attribution |
| `aura.orchestrator.iteration_complete` | Iteration finished with quality score, threshold, replan decision |
| `aura.orchestrator.replan_started` | Replan cycle triggered (coordinator-routed or task failures) |
| `aura.orchestrator.synthesizing` | Coordinator merging worker results (includes iteration number) |
| `aura.orchestrator.tool_call_started` | Tool execution began within a worker task |
| `aura.orchestrator.tool_call_completed` | Tool execution finished within a worker task |

### Orchestration Event Flow

```
User query received
       ↓
plan_created          ← goal, task_count, routing_mode, routing_rationale
       ↓
task_started          ← Worker assigned (task_id, worker_id, orchestrator_id)
       ↓
worker_reasoning      ← Worker thinking (task_id, worker_id, content)
       ↓
tool_call_started     ← Worker calls MCP tool (tool_call_id, tool_name, worker_id)
       ↓
tool_call_completed   ← Tool result (duration_ms, success)
       ↓
task_completed        ← Worker finished (duration_ms, success, result)
       ↓
synthesizing          ← Coordinator merging results (iteration)
       ↓
iteration_complete    ← Quality scored (quality_score, quality_threshold,
                         will_replan, evaluation_skipped, reasoning, gaps)
       ↓
If will_replan:
  replan_started      ← trigger: "coordinator" | "failure"
       ↓
  → loop back to plan_created
```

**Alternative routing**: The coordinator may emit `direct_answer` (simple queries) or `clarification_needed` (ambiguous queries) instead of `plan_created`, skipping the orchestration loop entirely.

### Orchestration Event Formats

**Plan created** (coordinator decomposed query into tasks):
```
event: aura.orchestrator.plan_created
data: {"goal":"Calculate (3+7)*2 and list files","task_count":2,"routing_mode":"orchestrated","routing_rationale":"Multi-step: arithmetic + file listing","agent_id":"coordinator","session_id":"sess_xyz"}
```

The `routing_mode` field indicates how the coordinator routed the query:
- `"routed"` — classified to a single worker (evaluation skipped, synthesis still runs)
- `"orchestrated"` — multi-task DAG with synthesis + evaluation

**Task started** (worker begins execution):
```
event: aura.orchestrator.task_started
data: {"task_id":0,"description":"Calculate (3+7)*2","worker_id":"arithmetic","orchestrator_id":"orch-1","agent_id":"coordinator","session_id":"sess_xyz"}
```

**Task completed** (worker finished with result):
```
event: aura.orchestrator.task_completed
data: {"task_id":0,"success":true,"duration_ms":1500,"orchestrator_id":"orch-1","worker_id":"arithmetic","result":"The result is 20","agent_id":"coordinator","session_id":"sess_xyz"}
```

**Worker reasoning** (worker thinking with attribution):
```
event: aura.orchestrator.worker_reasoning
data: {"task_id":0,"worker_id":"arithmetic","content":"I need to add 15 and 27...","agent_id":"coordinator","session_id":"sess_xyz"}
```

Worker reasoning is also emitted as `aura.reasoning` with `agent_id` set to the worker name (e.g., `"arithmetic"`) and `parent_agent_id: "coordinator"` for backward-compatible aggregation.

**Iteration complete** (passive quality evaluation; coordinator decides continuation):
```
event: aura.orchestrator.iteration_complete
data: {"iteration":1,"quality_score":0.85,"quality_threshold":0.7,"will_replan":false,"evaluation_skipped":false,"reasoning":"Response is complete and accurate","gaps":[],"agent_id":"coordinator","session_id":"sess_xyz"}
```

The `evaluation_skipped` field is `true` when a single-task plan completes successfully — the quality evaluation LLM call is skipped and `quality_score` defaults to `1.0`. Quality evaluation is passive (observability only); the coordinator decides whether to continue via its routing decision (`respond_directly`, `create_plan`, or `request_clarification`), not the quality score. `will_replan` reflects whether the iteration budget allows another cycle.

**Replan started** (new planning cycle triggered):
```
event: aura.orchestrator.replan_started
data: {"iteration":2,"trigger":"coordinator","agent_id":"coordinator","session_id":"sess_xyz"}
```

Triggers: `"coordinator"` (coordinator routed back to `create_plan`) or `"failure"` (worker task failures forced a replan).

**Synthesizing** (combining worker results):
```
event: aura.orchestrator.synthesizing
data: {"iteration":1,"agent_id":"coordinator","session_id":"sess_xyz"}
```

### Orchestration Correlation

| Correlation | Events | Field |
|-------------|--------|-------|
| Task lifecycle | `task_started` → `worker_reasoning` → `tool_call_*` → `task_completed` | `task_id` |
| Phase lifecycle | `phase_started` → `phase_completed` | `phase_id` |
| Tool lifecycle | `tool_call_started` → `tool_call_completed` | `tool_call_id` |
| Worker identity | `task_*`, `worker_reasoning`, `tool_call_started` | `worker_id` |
| Agent hierarchy | All orchestration events | `agent_id` (`"coordinator"` or worker name) |
| Replan cycle | `iteration_complete` → `replan_started` → `plan_created` | `iteration` |

## SSE Event Reference

### Event Types by Mode

| Event | Description | `none` | `open-web-ui` | `aura` |
|-------|-------------|:------:|:------------:|:------:|
| **Text chunk** | Token-by-token content | ✅ | ✅ | ✅ |
| **Tool call** | Tool name + arguments | ✅ (with args) | ✅ (empty args) | ✅ (with args) |
| **Tool result** | Tool execution output | - | ✅ (via tool_calls) | ✅ (via aura.tool_complete) |
| **Final chunk** | `finish_reason` + usage | ✅ | ✅ | ✅ |
| **[DONE]** | Stream termination | ✅ | ✅ | ✅ |

### Message Formats

**First text chunk** (includes `role`):
```json
{"choices":[{"delta":{"role":"assistant","content":"Hello"}}]}
```

**Subsequent text chunks**:
```json
{"choices":[{"delta":{"content":" world"}}]}
```

**Tool call (`none` mode)** - includes actual arguments:
```json
{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_xyz","type":"function","function":{"name":"list_files","arguments":"{\"path\":\"/tmp\"}"}}]}}]}
```

**Tool call (`open-web-ui` mode)** - empty arguments for UI compatibility:
```json
{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_xyz","type":"function","function":{"name":"list_files","arguments":""}}]}}]}
```

**Tool result (`open-web-ui` mode only)** - sent as second delta with same index:
```json
{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_xyz","type":"function","function":{"name":"","arguments":"{\"files\":[\"a.txt\",\"b.txt\"]}"}}]}}]}
```

**Final chunk**:
```json
{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}
```

**Stream end**:
```
data: [DONE]
```

### finish_reason Values

| Value | Meaning |
|-------|---------|
| `stop` | Normal completion |
| `tool_calls` | Response included tool execution |
| `length` | Response truncated due to max_tokens limit |

## Client Examples

### JavaScript

```javascript
const response = await fetch('/v1/chat/completions', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({
    messages: [{ role: 'user', content: 'List files in /tmp' }],
    stream: true
  })
});

const reader = response.body.getReader();
const decoder = new TextDecoder();

while (true) {
  const { done, value } = await reader.read();
  if (done) break;

  for (const line of decoder.decode(value).split('\n')) {
    if (!line.startsWith('data: ')) continue;
    const data = line.slice(6);
    if (data === '[DONE]') break;

    const chunk = JSON.parse(data);
    const delta = chunk.choices[0]?.delta;

    if (delta?.content) {
      process.stdout.write(delta.content);
    }
    if (delta?.tool_calls) {
      console.log('Tool call:', delta.tool_calls[0].function.name);
    }
  }
}
```

### Python

```python
import httpx
import json

with httpx.stream('POST', 'http://localhost:8080/v1/chat/completions',
    json={'messages': [{'role': 'user', 'content': 'Hello!'}], 'stream': True}
) as response:
    for line in response.iter_lines():
        if not line.startswith('data: '): continue
        data = line[6:]
        if data == '[DONE]': break

        chunk = json.loads(data)
        delta = chunk['choices'][0].get('delta', {})

        if content := delta.get('content'):
            print(content, end='', flush=True)
        if tool_calls := delta.get('tool_calls'):
            print(f"\nTool: {tool_calls[0]['function']['name']}")
```

## Multi-Turn Tool Execution

Unlike standard OpenAI API (where tool execution is client-side), this server executes tools server-side and continues streaming. After tool execution completes, text resumes with a `\n\n` separator for readability:

```
I'll check that for you.
[tool call: list_files]
[tool executes server-side]

Here are the files I found:
...
```

The separator is automatically injected when text chunks resume after a `ToolResult` event.

## Connection Behavior

| Behavior | Description |
|----------|-------------|
| **Timeout** | 900s default (configurable via `STREAMING_TIMEOUT_SECS`, 0 = disabled) |
| **Disconnect** | Server detects client disconnect and cancels in-flight operations |
| **Backpressure** | Bounded buffer prevents memory exhaustion |
| **Cancellation** | Timeout or disconnect triggers MCP tool cancellation via `notifications/cancelled` |

## Graceful Shutdown

On SIGTERM or SIGINT, the server performs a two-phase shutdown to let in-flight requests finish:

```
SIGTERM/SIGINT received
         │
         ▼
┌─────────────────────────────────────────┐
│ Phase 1: Gate (immediate)               │
│   • New requests rejected with 503      │
│   • In-flight streams continue running  │
└─────────────────────────────────────────┘
         │
         │  ← grace period (SHUTDOWN_TIMEOUT_SECS, default 30s)
         │    in-flight streams may complete naturally during this window
         ▼
┌─────────────────────────────────────────┐
│ Phase 2: Drain (after grace period)     │
│   • Remaining streams cancelled         │
│   • Each stream sends [DONE] to client  │
│   • MCP cleanup (cancel + close)        │
└─────────────────────────────────────────┘
         │
         │  ← 10s buffer for [DONE] delivery + MCP cleanup
         ▼
┌─────────────────────────────────────────┐
│ Server exits                            │
└─────────────────────────────────────────┘
```

| Phase | Timing | What happens |
|-------|--------|-------------|
| **Gate** | Immediate | Middleware returns 503 for all new requests (including `/health`) |
| **Grace period** | 0 – `SHUTDOWN_TIMEOUT_SECS` (default 30s) | In-flight streams continue running; streams that finish naturally are unaffected |
| **Drain** | After grace period | `stream_shutdown_token` cancelled; remaining streams send `[DONE]`, then MCP cleanup runs |
| **Exit** | Grace period + 10s buffer | Actix force-closes any remaining connections |

Configure the grace period:

```bash
# Allow 60 seconds for in-flight requests to finish
SHUTDOWN_TIMEOUT_SECS=60 cargo run --bin aura-web-server

# Or via CLI flag
cargo run --bin aura-web-server -- --shutdown-timeout-secs 60
```

**K8s tip**: Set `terminationGracePeriodSeconds` to at least `SHUTDOWN_TIMEOUT_SECS + 15` (default: 45s). The total shutdown budget is grace period + 10s Actix buffer. During Phase 1, `/health` returns 503 — readiness probes will fail immediately, removing the pod from service endpoints.

**Note**: The `/health` endpoint returns 503 during shutdown (same middleware gate as all routes). This is intentional — it signals load balancers and K8s readiness probes to stop routing traffic to this instance.

## Response Headers

```
Content-Type: text/event-stream
Cache-Control: no-cache
X-Accel-Buffering: no
```
