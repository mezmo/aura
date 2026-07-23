# S21 stream-liveness type design record

Baseline: aura `7a0f0651`, branch `card/S21`. Scope: the `stream_liveness`
module that lands mezmo/aura#394's inactivity-timeout proposal for worker
streams.

Motivation: `per_call_timeout_secs` wraps the WHOLE worker ReAct loop in
`stream_and_forward` (every turn and tool execution up to `max_depth`), so a
flat wall-clock bound cannot distinguish a hung provider from a busy worker.
The inactivity policy bounds silence instead: every stream item (text delta,
reasoning delta, tool call, tool result) re-arms the deadline, and the stream
fails only after N consecutive silent seconds.

Config: `[orchestration.timeouts] inactivity_timeout_secs`, default 0
(disabled; behavior unchanged unless configured). The "0 disables" decision
is parsed once by `StreamLiveness::from_secs`; downstream code never
re-checks a raw integer (same discipline as `bounding`'s `NonZeroDuration`).

Seam: one call in `stream_and_forward`'s item loop replaces the bare
`stream.next().await` with `liveness.next_item(&mut stream)`.
`stream_and_collect` (coordinator one-shot phases, `max_depth = 1`) is
untouched: its whole-call wrap is correctly named there. Provider-aware
policy (GPT/Chat-Completions silent reasoning) and the web-server
first-chunk half of #394 are out of scope.

## Type inventory

Every public type maps to one business rule and names the invalid state it
forbids.

| Type | Business rule | Forbidden invalid state |
|---|---|---|
| `StreamLiveness` | The liveness policy is decided once from config: 0 disables, positive bounds silence between items | An "enabled with zero seconds" policy; the enabled/disabled decision and the window are one value |
| `InactivityWindow` | The window bounds silence between consecutive items, not total duration; any item re-arms the deadline | A zero window ("time out immediately"): zero parses to `Unbounded` instead |
| `InactivityElapsed` | The stream failed because no item arrived within the window; carries the window for the error message | An inactivity error without its window |
