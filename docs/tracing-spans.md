# Tracing Span Layout

Aura exports OpenTelemetry spans via OTLP when `OTEL_EXPORTER_OTLP_ENDPOINT`
is set. Spans follow the [OpenInference](https://github.com/Arize-ai/openinference)
taxonomy so they render correctly in Phoenix, Jaeger, and similar tools.

## Trace structure

Every request produces two traces:

1. **HTTP trace** — covers the request/response lifecycle
2. **Agent trace** — covers the LLM/tool execution

The agent trace is rooted at `agent.stream` with `parent: None` so Phoenix
sees it as an independent trace root with all LLM I/O attributes.

### HTTP trace (both modes)

```text
chat_completions (CHAIN)
  └── streaming_completion (CHAIN)
```

### Single-agent mode

```text
agent.stream (AGENT, ROOT)
  └── agent.turn (LLM)
      ├── execute_tool (TOOL)
      │   └── mcp.tool_call (TOOL)
      └── execute_tool (TOOL)
          └── mcp.tool_call (TOOL)
```

### Orchestration mode

```text
agent.stream (AGENT, ROOT)
  └── orchestration (CHAIN)
        ├── orchestration.planning (CHAIN)
        │   └── agent.turn (LLM) → execute_tool → mcp.tool_call
        └── orchestration.iteration (CHAIN)
            ├── orchestration.worker (AGENT)
            │   └── agent.turn (LLM) → execute_tool → mcp.tool_call
            ├── orchestration.synthesis (CHAIN)
            │   └── agent.turn (LLM) → ...
            └── orchestration.evaluation (CHAIN)
                └── agent.turn (LLM) → execute_tool → mcp.tool_call
```

## Span attributes

### Agent root (`agent.stream`)

`user.id`, `session.id`, `metadata`, `input.value`, `output.value`,
`llm.token_count.prompt`, `llm.token_count.completion`, `llm.token_count.total`

### Orchestration spans

| Span | Attributes |
|------|-----------|
| `orchestration` | `orchestration.goal`, `orchestration.max_iterations`, `orchestration.routing` (direct/clarification/orchestrated) |
| `orchestration.planning` | `orchestration.phase` |
| `orchestration.iteration` | `orchestration.iteration`, `orchestration.task_count`, `orchestration.quality_score`, `orchestration.will_replan` |
| `orchestration.worker` | `orchestration.task_id`, `orchestration.worker`, `orchestration.task` |
| `orchestration.synthesis` | `orchestration.phase`, `orchestration.completed_tasks` |
| `orchestration.evaluation` | `orchestration.phase`, `orchestration.quality_score` |

Token usage (`llm.token_count.*`) is recorded on all orchestration phase
spans (planning, worker, synthesis, evaluation).

## OpenInference span kinds

Span kind is inferred from the span name by the custom exporter in
`openinference_exporter.rs`:

| Kind | Spans |
|------|-------|
| **LLM** | `chat_streaming`, `agent.turn` |
| **TOOL** | `execute_tool`, `mcp.tool_call` |
| **AGENT** | `agent.stream`, `orchestration.worker` |
| **CHAIN** | `chat_completions`, `streaming_completion`, `orchestration`, `orchestration.planning`, `orchestration.iteration`, `orchestration.synthesis`, `orchestration.evaluation` |

## Span parenting

- `agent.stream` is created with `parent: None` to break the link from the
  HTTP handler trace, making it an independent trace root in Phoenix.
- The `tokio::spawn` in `handlers.rs` is instrumented with `agent.stream` so
  Rig's `agent.turn` becomes a direct child.
- In orchestration mode, `Orchestrator::stream()` instruments its spawned task
  with the `agent.stream` span so all orchestration child spans nest under the
  trace root.
- `ToolWrapper::call` propagates the current span into its `tokio::spawn` so
  `mcp.tool_call` nests under Rig's `execute_tool`.

## Content recording

When `OTEL_RECORD_CONTENT=true`, prompt/completion text and tool
arguments/results are recorded as span attributes, truncated to
`OTEL_CONTENT_MAX_LENGTH` (default 1000 bytes, rounded to a UTF-8 boundary).

## Known limitations

- **Tool error propagation**: Tool errors are only recorded on the
  `mcp.tool_call` child span (by `mcp_tool_execution.rs`), not on Rig's
  `execute_tool` parent. This is intentional — `mcp.tool_call` is the
  canonical TOOL span for Phoenix.
