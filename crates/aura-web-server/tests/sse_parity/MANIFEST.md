# SSE and span parity: coverage manifest

`tests/sse_parity.rs` serves one chat completion through both chat paths
and compares what they produce.

The request-bound path (`execute_completion` reading the agent's item
stream) is the oracle. It stays in the code until
[mezmo/aura#627](https://github.com/mezmo/aura/issues/627) removes it, so
the test runs it live instead of comparing against recorded output.

The runtime path runs these steps:

- build an `AgentRuntime::over_agents` that serves the mock (behind
  `aura`'s `test-support` feature, which this crate's dev-dependency turns
  on);
- attach an observer that claims the session with presence;
- call `start`, handing it the run's `agent.stream` span;
- read the session's stream up to the run's `Started`;
- render the rest with `sse_body`, using an `SseProjector` built from the
  same `StreamConfig` and `TurnContext` as the oracle, and
  `Timeouts::none()`.

Both paths run the same `MockAgent` script with a run context
(`with_run_context()`). The tests use paused time on a single
current-thread tokio runtime, so the thread-default subscriber that
`CapturedSpans::install` sets up sees the spans from both paths.

## The tests

| Group | What it runs | State |
|---|---|---|
| `original_path::<script>` | The oracle, twice per variant. The body must end with `[DONE]`, hold every mark the script lists for the variant (`Mark::Has`, `Lacks`, `Before`), and render the same on the second run. | Green. |
| `runtime_span::<script>` | The runtime path's `agent.stream` span, compared with the oracle's. The test reads the session's stream itself until the run ends, and builds no body. The run's task records the span, so a failure here is in the span alone. | Green. |
| `both_paths::<script>` | Both paths through to the body, for every variant. Bodies are compared byte for byte and spans as trees. A mismatch writes both sides to `$CARGO_TARGET_TMPDIR/sse_parity/<script>.<variant>.{sse,spans}.{request-bound,runtime}` and reports the first line that differs. | Green. |
| `controls::*` | Checks the comparison and its rewrites on inputs whose differences are known. | Green. |

Variant sets:

- `PLAIN`: custom events on, and off.
- `TOOLS`: custom events on and off, each with all three `ToolResultMode`s.
  `Aura` renders the same as `None`. It stays in the set so that a
  projection that treats the two differently is caught.
- `REASONING`: custom events with reasoning, custom events without
  reasoning, and reasoning without custom events.
- `DEBUG_ERRORS`: custom events on and off, each with
  `debug_provider_errors` on and off.

Every variant uses the same `tool_result_max_length` (48), a fixed
completion id, model string and timestamp, the same correlation (session
`cs_parity` and a fixed trace id), the caller's `user`, and one metadata
key.

## Scripts

| Script | Agent | Variants | Drives |
|---|---|---|---|
| `text_and_final` | single | `PLAIN` | Two text deltas, `Final` with usage. |
| `tool_turn` | single | `TOOLS` | Text, a tool call (item, then `ToolRequested`, as rig orders them), an MCP `ToolStart` with a progress token, `ToolProgress`, a result longer than `tool_result_max_length`, a `TurnUsage` item, text after the tool, `ToolUsage`, `Final`. |
| `tool_outcomes` | single | `TOOLS` | Three calls: an `Error` result wrapped in rig's prefixes, text output that reports an MCP error, and `Json` output. |
| `unparsed_tool_arguments` | single | `TOOLS` | A call whose arguments are cut short. The tool call item has the raw text. `ToolRequested` has `{}` with the raw text beside it, which is what the streaming hook sends for arguments that do not parse. |
| `fallback_tool_call` | single | `TOOLS` | An Ollama model's tool call written as text, in the fallback executor's order: the text, the call's item, the `ToolRequested` the executor sends after it, the result, `Final`. No `ToolStart`, because the executor calls MCP without a tool call id. |
| `reasoning_then_text` | single | `REASONING` | Two reasoning deltas, text, `Final`. |
| `final_with_cache_split` | single | `PLAIN` | Text, `Final` with usage and a cache split. |
| `provider_error` | single | `DEBUG_ERRORS` | Text, then a stream error. |
| `context_overflow` | single | `PLAIN` | A context-overflow error as the first item. |
| `passthrough_tool` | single, client tools | `TOOLS` | Text, a client tool's call and `ToolRequested`, the passthrough marker as its result, the stream error that follows a yield. |
| `max_tokens_length` | single, `max_tokens` 5 | `PLAIN` | `Final` whose completion tokens reach `max_tokens`. |
| `approvals` | single | `PLAIN` | A tool call held for approval: approval requested, pending and completed, then `ToolStart` without a progress token, then the result. |
| `mcp_status_at_start` | single, two MCP servers | `PLAIN` | One connected and one failed server reported at start. |
| `scratchpad_tail` | single | `PLAIN` | `Final`, then the single agent's `ScratchpadUsage` item. |
| `scratchpad_tool_hidden` | single | `TOOLS` | A scratchpad exploration tool's call and result as items, with no `ToolRequested`, because the streaming hook does not announce scratchpad tools. |
| `orchestration_events` | orchestrated | `REASONING` | One `steps::emitted` step per orchestrator emit (the item on the stream and its event on the channel), coordinator reasoning and text included: `McpStatus`, `PlanCreated`, worker `TaskStarted`, `ToolStart`, `ToolComplete`, `Reasoning`, worker `ContextUsage` and `ScratchpadUsage`, `TaskCompleted`, `IterationComplete`, `ReplanStarted`, `Synthesizing`, the conversation's `ContextUsage` (`main`), `Final` with zero usage. |
| `orchestration_direct_answer` | orchestrated | `PLAIN` | `DirectAnswer`, text, `Final`, each through `steps::emitted`. |
| `orchestration_clarification` | orchestrated | `PLAIN` | `ClarificationNeeded` with options, text, `Final`, each through `steps::emitted`. |
| `orchestration_parks` | orchestrated | `PLAIN` | `PlanCreated`, `TaskStarted`, `TaskBlocked`, `RunParked`; the stream ends without `Final`. |
| `orchestration_event_ahead_of_content` | orchestrated | `REASONING` | Coordinator reasoning sent to both streams, then `PlanCreated` sent on the channel before the stream yields its item. This is the order the orchestrator produces when its item stream lags behind its emitter. |

## Surfaces and the scripts that cover them

The rows follow the projection's rules, one row per kind of session event.
"Gated" means the frame is sent only with custom events on. The `Lacks`
marks check that a gated frame is absent in the variants with custom
events off.

| Surface | Frame | Covered by |
|---|---|---|
| `Started` renders nothing | (none) | every script: the body starts after it |
| `SessionInfo` | `aura.session_info`, gated | every script |
| `McpStatus`, single agent, opening | `aura.mcp_status`, gated, after `session_info` | `mcp_status_at_start` |
| `McpStatus`, orchestration, mid-stream | `aura.mcp_status`, gated | `orchestration_events` |
| `TextDelta`, first-chunk role | content chunk with `role` | `text_and_final`, every script with text |
| separator after a tool result | `\n\n` before the next text | `tool_turn` |
| `Reasoning { task_id: None }` | `aura.reasoning`, needs custom events and reasoning | `reasoning_then_text`, `orchestration_events`, `orchestration_event_ahead_of_content` |
| `Completed` | final chunk's `usage`; the span's output | every script that ends with `Final` |
| `ToolRequested` | `tool_calls` chunk (arguments per mode), then `aura.tool_requested`, gated | `tool_turn`, `tool_outcomes`, `fallback_tool_call`, `passthrough_tool`, `approvals` |
| `ToolRequested` for arguments that do not parse | the chunk carries the provider's text, `aura.tool_requested` carries `{}` | `unparsed_tool_arguments` |
| `ToolStart { task_id: None }` | `aura.tool_start`, gated, with and without a progress token | `tool_turn`, `approvals` |
| `ToolProgress` | `aura.progress`, gated | `tool_turn` |
| `ToolUsage` | `aura.tool_usage`, gated | `tool_turn` |
| `ApprovalRequested`, `ApprovalPending`, `ApprovalCompleted` | `aura.approval_*`, ungated | `approvals` (custom events off too) |
| `ToolComplete { task_id: None }`, success, result cut to `tool_result_max_length` | `aura.tool_complete`, gated | `tool_turn` |
| failure from an error result, rig's prefixes stripped | `aura.tool_complete` with `error` | `tool_outcomes` |
| failure reported in output | `aura.tool_complete` with `error` | `tool_outcomes` |
| JSON output | `aura.tool_complete` `result`; OpenWebUI chunk sends it unquoted | `tool_outcomes` |
| OpenWebUI result chunk | second `tool_calls` delta at the call's index | `tool_turn`, `tool_outcomes` (`OpenWebUI` variants) |
| passthrough result | no frame; `finish_reason: "tool_calls"`; marker never on the wire | `passthrough_tool` |
| scratchpad tool | no chunk, no `aura.tool_complete`, no separator | `scratchpad_tool_hidden` |
| `TurnUsage` item | nothing | `tool_turn` |
| `Usage`, taken from `Final` when it reports prompt tokens | `aura.usage`, gated, with the cache split | `text_and_final`, `final_with_cache_split` |
| `ContextUsage`, orchestration (a worker's and `main`) | `aura.context_usage`, ungated, named by the item's agent | `orchestration_events` |
| `ScratchpadUsage`, single agent's tail | `aura.scratchpad_usage`, ungated | `scratchpad_tail` |
| `ScratchpadUsage`, orchestration worker | `aura.scratchpad_usage`, ungated | `orchestration_events` |
| `PlanCreated`, `TaskStarted`, worker `ToolStart`, `ToolComplete`, `Reasoning`, `TaskCompleted`, `IterationComplete`, `ReplanStarted`, `Synthesizing` | `aura.orchestrator.*`, gated; worker reasoning also as `aura.reasoning`, needs reasoning | `orchestration_events` |
| `DirectAnswer` | `aura.orchestrator.direct_answer`, gated | `orchestration_direct_answer` |
| `ClarificationNeeded` | `aura.orchestrator.clarification_needed`, gated | `orchestration_clarification` |
| `TaskBlocked`, `RunParked` | `aura.orchestrator.task_blocked`, `run_parked`, gated | `orchestration_parks` |
| orchestration event and coordinator content, each sent in one emitter call | frames in the item stream's order | `orchestration_events` |
| orchestration event on the channel ahead of its buffered item | frames in the item stream's order | `orchestration_event_ahead_of_content` |
| `Finished` | final chunk `stop`, `[DONE]` | every finishing script |
| `Finished`, max tokens reached | `finish_reason: "length"` | `max_tokens_length` |
| `Finished` after a yield | `finish_reason: "tool_calls"` | `passthrough_tool` |
| `Parked` | final chunk, `[DONE]` | `orchestration_parks` |
| `Failed`, provider error, debug off and on | error text chunk, final chunk, `[DONE]` | `provider_error` |
| `Failed`, context overflow | overflow guidance as the first chunk | `context_overflow` |

The span, root `agent.stream` only (the mock opens no child spans):

| Surface | Covered by |
|---|---|
| `llm.system`, `llm.provider`, `llm.model_name`, `input.mime_type`, `input.length`, `http.request_id`, `session.id`, `aura.version`, `aura.mode` (both modes), `user.id`, `metadata`, `llm.invocation_parameters`, `message_count`, `output.mime_type`, `output.length`, in the order recorded | every script |
| status ok | every finishing script, `passthrough_tool` |
| status error with the stream's error text, and its `exception` event | `provider_error`, `context_overflow` |
| a park: `stream.termination = parked`, no status | `orchestration_parks` (rewritten on the oracle's side, see below) |

## Not covered, and why

| Surface | Why no script covers it | Owned by |
|---|---|---|
| `aura.skills_rehydrated` | Needs skill directories on disk, store records, and an agent that reports its skills. The mock reports none. | The runtime's opening-sequence test in `crates/aura/src/runtime/tests.rs`; `aura_events_test.rs` for the request-bound path (Docker). |
| single-agent `aura.context_usage` at the end, and `aura.usage` from the hook's totals | The mock's `UsageState` stays zero, so neither fires on either path. | The `billed_usage` suite in `src/streaming/handlers.rs`; the runtime's usage tests in `crates/aura/src/runtime/tests.rs`. |
| `aura.session_info`'s context limit | The mock reports no context window. | The projector's tests in `src/streaming/projection.rs`. |
| observer lifecycle events, another run's events, an `Unknown` journal item | Each test has one run per session. The attach event is read before the body starts, and the detach is written after the body ends. | The projector's tests in `src/streaming/projection.rs`. |
| `Cancelled { Unclaimed }` | A client going away is about the connection, not the bytes of the body. | The runtime's observer tests in `crates/aura/src/runtime/tests/observe.rs`; the Docker cancellation suite. |
| `Cancelled { Deadline, External, Shutdown, Unknown }` | The point in a script where a cancel lands is not fixed, and the deadline is time-driven. | The body's tests in `src/streaming/body.rs`, one per `Ending`. |
| `JournalItem::Gap` | Needs a subscriber that falls further behind than the store keeps. | The body's tests in `src/streaming/body.rs`. |
| heartbeats, the first-chunk and inactivity windows, `Pulse`, `RunActivity` | Time-driven. Known differences on the runtime path: an orchestrated run's MCP status item does not close the first-chunk window, and a scratchpad tool call does not pause the inactivity window. | The body's tests in `src/streaming/body.rs`, ported from the `inactivity` suite. |
| the run's deadline (`streaming_timeout_secs`) | Time-driven, and no script sets a timeout. Known difference: when a single agent reaches its timeout, the request-bound path ends with the provider-error text, and the runtime path ends as `Cancelled { Deadline }` with no error text. | The runtime's deadline tests in `crates/aura/src/runtime/tests.rs`; the body's tests in `src/streaming/body.rs`. |
| `WorkerPhase` | No producer. | None needed. |
| the scratchpad debug override (`AURA_EMIT_SCRATCHPAD_TOOL_EVENTS`) | An environment variable this test never sets. | The hook and `handle_tool_call` tests. |
| the HTTP layer: status codes, headers, 409, 503, 500 before `Started` | The test measures both paths where the bytes are produced, below the HTTP layer. | The handler tests in `src/handlers.rs`. |
| the non-streaming body (`collect_body`, `build_json_response`) | Not an SSE body. | The handler tests and the `build_json_response` tests in `src/handlers.rs`. |
| `input.value`, `output.value`, `gen_ai.system_instructions` | Recorded only when the process-wide `OTEL_RECORD_CONTENT` switch is on. The test leaves it as the environment sets it, normally off, and both paths see the same setting. The mock also has no system prompt. | `aura::logging`'s tests; the `spans` tests in `crates/aura/src/runtime/tests.rs`. |
| `llm.tools` | A built agent records none; only a prepared single agent has schemas. | The `spans` tests in `crates/aura/src/runtime/tests.rs`. |
| child spans (`agent.turn`, `mcp.tool_call`, `orchestration.*`), the `chat_completions` trace, preparation under the caller's span | The mock opens no spans, the request spans sit outside the `agent.stream` tree, and the test starts runs outside any span. | The Docker suites; the `spans` tests in `crates/aura/src/runtime/tests.rs`; the handler tests. |
| span log events other than `aura::logging`'s | Each path's own log lines, not span contents. | None needed. |
| `duration_ms` of a single agent's `aura.tool_complete` | Rewritten before comparing (below). | The `tee_run_events` tests in `crates/aura/src/streaming.rs`; the `tool_events` suite in `src/streaming/handlers.rs`. |

## Rewrites before comparing

Each rewrite targets the exact frame or attribute that holds the value, and
checks it first. The pass panics if what it expects to find is not exactly
there.

| Rewrite | Where | Audit | Why |
|---|---|---|---|
| Heartbeat comments dropped | Whole frames equal to `: heartbeat\n\n` | Exactly one on the oracle, none on the runtime path | The request-bound loop's heartbeat is a `tokio::time::interval`. Its first tick fires at once, so the loop always sends one heartbeat, whatever the interval, and no setting the loop takes avoids it. In a script that opens with an event, that tick races the event's frame in the loop's unbiased `select!`. The harness sets the interval to a day (`QUIET`), far longer than a script runs under paused time, so no second tick comes. The audit's count of exactly one checks that. The runtime body sends no heartbeat under `Timeouts::none()`. Once that one frame is dropped, the oracle renders the same on every run: the positive control below renders every variant twice and requires the two to agree. |
| `duration_ms` becomes `<elapsed>` | `aura.tool_complete` frames only. The orchestration frames' durations come from the script, so they are compared as they are. | Exactly one `"duration_ms":` per such frame, followed by digits | Both paths measure it on the wall clock (`std::time::Instant`), which paused tokio time does not control. |
| `code.*`, `thread.id`, `thread.name`, `busy_ns`, `idle_ns` dropped | Every span | Exactly one of each (`thread.name` at most one) | Each path creates its span at its own call site, on its own schedule. |
| `run.id`, `http.request_id` become `<run>` | The `agent.stream` root | Exactly one of each, equal to that path's run id | Each path's run has its own id. |
| Span events kept only from `aura::logging` | Every span | No span dropped an event | The rest are each path's log lines. |
| The oracle's park ending becomes the runtime's | The root, `orchestration_parks` only | The oracle's status is ok, with no `stream.termination` | The runtime's termination table (`termination` in `crates/aura/src/runtime/otel.rs`) records a park as `stream.termination = parked` with no status. The request-bound path has no park ending and records the stream as complete. |

Everything else is compared exactly as produced. The completion id, model
string, timestamp and correlation are fixed by the test, so they are the
same on both sides.

## Controls

- Positive: every `original_path` test renders each variant twice and
  requires the two to agree, which shows the oracle is deterministic under
  the harness. `both_paths` agrees on every script and variant, for bodies
  and spans.
- Negative: `controls::a_changed_byte_is_reported_at_its_line` changes one
  byte in a body and expects one failure that names the line.
  `controls::a_changed_attribute_is_reported_in_the_spans` does the same
  for a span attribute.
- Audits: the three `should_panic` controls show that the body checks
  refuse a wrong heartbeat count, a frame cut short, and a `tool_complete`
  frame with no duration. `only_the_tool_complete_duration_is_rewritten`
  shows that the duration rewrite leaves an orchestration frame's duration
  alone.

## Differences between the paths

None for these scripts: every `both_paths` test agrees on every variant.
The known differences between the paths are time-driven, so no script
reaches them. They are listed under "Not covered", in the rows for the
windows and the run's deadline.

## Residual risks

- `steps::emitted` keeps its own copy of the mapping that
  `aura::orchestration::emitter` uses to turn an item into an event,
  because the emitter's mapping is private to `aura`. If the emitter's
  attribution changes, this test notices only when the two copies produce
  different frames.
- The mock settles between steps, so the item stream never holds more than
  one item ahead of its consumer. Buffering is modelled only where a script
  does it explicitly (`orchestration_event_ahead_of_content`).

## What a pass proves

A green `both_paths` suite proves two things for these scripts and
variants:

- after the two body rewrites above, the runtime path's SSE body is byte
  for byte the same as the request-bound path's;
- after the span rewrites, its `agent.stream` span has the same name, kind,
  status, attributes (in order) and `aura::logging` events.

It proves nothing else. It does not cover timing (heartbeats, windows, tool
durations), the connection's lifecycle (disconnect, shutdown, the unclaimed
cancel), the HTTP layer, the non-streaming body, event orders that
production can produce but the scripts do not, or anything downstream of
the bytes.

## Hole inventory

No test in the file is ignored, so the following returns nothing:

```
grep -n '#\[ignore' crates/aura-web-server/tests/sse_parity.rs
```
