# P45 stage 4 design record — the parked control outcome producer (R7)

Mechanism spec for the producer half of R7 (Mike, 2026-09-12): the park arm
stops returning the fake "did not run" tool result — the sentinel is never
model-visible truth; a distinct PARKED CONTROL OUTCOME replaces it; genuine
sibling gated calls from the same turn still register. The purpose is on the
record in the Gate M evidence: today the model reasons from the fake result
and pivots to alternative calls; executing both later may run mutually
alternative actions
(`…/evidence/2026-09-12-p45-fold-gate-m/README.md:58-71`,
`deny-leg.sse:113-114`).

Sources: the R7 ruling; the Gate M evidence set (deny-leg SSE, the resuming
document); `REBUILD-DESIGN.md`'s binding condition (residual risk (a): the
capture must keep `current_prompt` a tool-result user message or the stage 2
preflight refuses); and the mapped facts of Part 1 below, each with
file:line. Stage 4b implements this spec as frames; no production change
lands in this unit.

## Ruling addendum — Option A RULED (Mike, 2026-09-12); Option B rejected for now

Mike ruled the implementation shape on 2026-09-12, between this spec's two
options, with two confirmed assumptions:

1. **A cancelled or timed-out approval NEVER executes the tool** — true
   today at every layer (the park arm registers without executing; the
   orphan sweep cancels undecided tickets; a resolved cancellation is
   terminal). The stage 4b frames must not disturb it; they assert it
   (`gated_invocations` stays empty in every producer frame).
2. **NO model call after a park** — a TRUE cancel after the record: the
   approval is recorded (register + cell push), the current batch drains
   (gated siblings register, ungated siblings execute), and the hard
   cancel fires before the next `stream_completion`.

**OPTION A (RULED, as built in stage 4b): the minimal hard-cancel.** One
production change, in `streaming_request_hook.rs`'s park branch alone: the
snapshot still lands first (capture before cancel, always), then the HARD
flag is set through the fork's existing `cancel()`. The fork's post-hook
`is_cancelled` check then yields the park `Err` before
`stream_completion` — no completion ever runs over the sentinel. The
reason is carried by the hook's own log line and by the blocked cell,
NOT by `cancel_with_reason`: the hook receives a `CancelSignal` CLONE
whose `Arc` shares only the hard flag — the reason `OnceLock` is
clone-local (`prompt_request/mod.rs:161-200`), so a stamp on it never
reaches the loop's `Err` (which reads `<no reason given>` either way),
and the call was dropped as provably ineffective (Gate A, stage 4b).
Everything else is untouched: the gate still short-circuits with the
sentinel (it is still a tool RESULT in the batch's aggregated prompt),
`PreCallOutcome`, `tool_wrapper.rs`, and the rig fork pin are all
unchanged. The sentinel is therefore still conversation-adjacent, but
never model-visible: no completion follows the parking batch, and the
resume replaces every slot with a real decision outcome. Under Option A
the capture needs NO synthesis — the parking batch's sentinel slots
aggregate into the fork-built prompt exactly as today, so same-message
parks capture one slot per call natively, and the cross-turn one-slot-total
pivot shape becomes UNPRODUCIBLE (no turn can follow a park). Legacy
one-slot documents stay accepted by the Stage 2 builder — which is the
entire reason Option B is unnecessary for correctness.

**OPTION B (this document's sections 1-4 and 7 record it; REJECTED FOR
NOW): the fork tri-state arm** — `PreCallOutcome::Parked` crossing the
tool-server boundary as a typed `ToolError::Parked` the fork's loop arm
recognizes (bookkeep the call, produce no result, drain the batch), plus
capture-side slot synthesis. Rejected for now: cross-repo blast radius — a
pinned-fork change (an enum variant plus a loop arm in mezmo/rig) — for a
difference the Stage 2 builder already absorbs (it keys on call ids and
replaces slots, so a never-model-visible sentinel slot is functionally
equivalent to a synthesized one). It may return as a fork cleanup if the
sentinel-free crossing is ever needed — for example to also retire the
client-visible fabricated `tool_call_completed(success: sentinel)` event
(Option A leaves that event surface as is).

The frames that landed under Option A (stage 4b, in the orchestrator's
test module beside the orphan pair): the four producer frames —
`no_completion_runs_after_a_park_and_the_sentinel_never_reaches_the_model`,
`one_message_two_gated_calls_both_park_and_the_snapshot_carries_a_slot_per_call`,
`a_mixed_batch_parks_the_gated_call_and_executes_the_ungated_sibling`,
`a_blocked_cell_never_re_drives_the_worker_attempt_loop` — plus the
unchanged-by-verification backward-compat set (the full goldens resume
suite including the three Stage 1 pivot frames, the web-server suite, and
both wire-calibration frames, all green unedited).

Every claim below is against the workspace tip `ff9dfa7a` and the pinned rig
fork (mezmo/rig @ `097d08d6`,
`…/checkouts/rig-4b637330f90271cd/097d08d/rig/rig-core/src/agent/prompt_request/`).

## Part 1 — the mapped producer flow (the premises)

### The park, step by step, today

1. The rig fork's streaming multi-turn loop consumes one assistant turn as a
   provider stream and executes its tool calls sequentially inside the turn
   (`streaming.rs:320-466`; the sequential guarantee is the fork contract,
   `test_rig.rs:6-14` and `docs/rig-fork-changes.md:36-46`). Per tool call:
   the hook's `on_tool_call` stashes the call id for the park arm
   (`streaming_request_hook.rs:573-577`), then the tool server runs the
   wrapped tool.
2. The gate's park arm (`gate.rs:131-261`), reached when a glob matches
   (`gate.rs:271-273`) and no recorded decision rules the call
   (`gate.rs:280-300`): registers durably, fail-closed (`gate.rs:205-214`);
   takes the stashed call id (`gate.rs:216-223`); records the guard
   (`gate.rs:232`); publishes requested/pending (`gate.rs:235-249`); appends
   the `PendingCall` to the blocked cell (`gate.rs:251`); returns
   `PreCallOutcome::ShortCircuit { output: PARK_SENTINEL }` (`gate.rs:258-260`,
   the literal at `gate.rs:24-26`).
3. The wrapper turns that into a SUCCESSFUL tool result: `ShortCircuit` →
   `return Ok(output)` with an `on_complete(Ok(&output))` side-firing
   (`tool_wrapper.rs:530-549`) — which is also why the deny leg's SSE shows
   `tool_call_completed` with success and the sentinel text
   (`deny-leg.sse:102-103`). The rig loop takes the Ok as the call's result
   (`streaming.rs:367-374`), pushes it into the turn's `tool_results`
   (`streaming.rs:392`), yields a `ToolResult` item, and — nothing cancels
   mid-batch — proceeds to the next call in the same message.
4. At end of turn: the assistant message (with its tool calls) is pushed to
   the loop-owned `chat_history` (`streaming.rs:481-508`); all results are
   aggregated into ONE user message (`streaming.rs:510-535`) which is popped
   to become the next turn's `current_prompt` (`streaming.rs:537-541`).
5. The next `'outer` iteration fires `on_completion_call` before anything
   else (`streaming.rs:276-279`). The aura hook's park branch snapshots and
   stamps (`streaming_request_hook.rs:496-508`):
   `cell.snapshot_if_pending(history, prompt)` — first capture wins
   (`types.rs:602-619`) — then `cancel_sig.cancel_with_reason("parked")`.

### The resolution of the apparent contradiction

**The park "cancel" cancels nothing.** In the fork, `cancel_with_reason`
sets ONLY the reason — it never touches the cancelled flag — while
`is_cancelled()` reads only the flag
(`prompt_request/mod.rs:178-186`); the hard flag is set exclusively by
`cancel()` (`mod.rs:174-176`, used by the external-cancel and timeout path
via `check_and_cancel`, `streaming_request_hook.rs:466-478`). So the fork's
`if cancel_sig.is_cancelled()` after the hook (`streaming.rs:281-285`) is
false, no `Err` is yielded, and the loop falls straight through to
`agent.stream_completion(current_prompt, history)` (`streaming.rs:306-313`).
**The completion after a park is always issued, with the sentinel in
`current_prompt`.** The model reads the fake result as truth and pivots —
the deny leg's second call carries the pivot verbatim in its arguments:
`_aura_reasoning: "Since the relative time call was parked pending approval,
using the absolute time range equivalent…"` (`deny-leg.sse:113-114`).

### The four answers

**(a) One assistant message, two gated calls.** Both execute sequentially
(step 1-3): call A registers, parks, sentinel A becomes its tool result;
the between-tool cancel checks (`streaming.rs:357-362, 378-387`) read the
hard flag, which nothing has set, so call B registers, parks, sentinel B
becomes its result. The turn's assistant message (both calls) enters
history; both sentinels aggregate into one user message popped as
`current_prompt` (`streaming.rs:481-541`). At the next completion the hook
captures `(history, current_prompt)` — with ONE sentinel slot PER parked
call — and stamps the inert reason. The completion then issues anyway
(the resolution above): the model is re-driven with both sentinels in
context. So same-message parks snapshot one-per-call TODAY; the
`duplicate_key_document` fixture already stages this genuine shape
(`GOLDENS.md:64, 118-121`).

**(b) Where the re-drive happens.** In the rig fork's `'outer` loop
(`streaming.rs:228`) — the SAME multi-turn loop, the very next iteration,
within ONE `execute_task` worker stream (`orchestrator.rs:3907-3926`) — not
in the orchestrator's attempt loop and not on resume. The conversation is
the loop-owned `chat_history` (grown per turn, steps 1-4) plus
`current_prompt` = the aggregated tool-result user message carrying the
sentinel(s). The Gate M pivot happened in the ORIGINAL live run (parked_at
21:35:39, before any decision): first park in turn 1 → snapshot captured at
the turn-2 completion (the resuming document's shape proves it: history =
[user, assistant-with-call-1], current_prompt = one sentinel keyed
`tooluse_xu3VBf5c…` — `deny-leg-resuming-document.json:73-129`) → the
inert-cancel completion issued → the model saw the sentinel, pivoted to
`…_time_range`, which parked as `tooluse_WOxES4w8…`.

**(c) What ends the re-driven stream.** Nothing park-specific. The deny leg
ended it when the model called `submit_result` (`deny-leg.sse:137-141`),
tripping `drive_forward_loop`'s decision-ready short-circuit
(`orchestrator.rs:1543-1551`); a text-only turn ends it via the Final arm
(`streaming.rs:543-562`); depth, external hard-cancel, and timeout are the
other exits. After the second park the hook's `snapshot_if_pending` returns
false (first capture won) so the hook takes no park action at all — the
model kept reasoning over both sentinels until it terminated itself. The
cell then reads `Blocked { pending: every park the stream accumulated,
snapshot: the one captured after the FIRST park }`
(`orchestrator.rs:3931-3949`), the run parks, and the commit publishes
(`commit.rs:131-168`).

**(d) One sentinel slot, not two.** `snapshot_if_pending` never overwrites
(`types.rs:611-613`) and is called only from `on_completion_call`; the
capture froze at the FIRST completion after the FIRST park. The pivot's
second call parked in a LATER turn — after the freeze — so its
`PendingCall` rides the cell (and the checkpoint's pending list,
`deny-leg-resuming-document.json:50-71`) but neither its sentinel nor its
assistant turn ever entered the snapshot. Cross-turn pivots produce
one-slot-total; same-message batches produce one-per-call. The Gate M
deny leg is the cross-turn case; the fold's replace-miss fault is its
downstream scar.

## Part 2 — the mechanism spec

### 1. The control outcome

> **Superseded by the ruling addendum**: this section (and the
> fork-crossing parts of sections 3, 4, and 7) is the OPTION B record.
> Option A — the minimal hard-cancel, with the gate's sentinel kept and
> the capture unchanged — is what Mike ruled and stage 4b built; see the
> addendum at the top of this file.

`PreCallOutcome` (`tool_wrapper.rs:172-184`) gains:

```rust
/// The call parked for a human decision: registered durably, appended to
/// the blocked cell, awaiting a decision. NOT a tool result — no output
/// exists. Producing the slot the checkpoint needs is capture-side
/// bookkeeping (the snapshot synthesizes it at capture); the sentinel is
/// never model-visible truth.
Parked,
```

Payload: unit. The parked call's identity (decision id, tool, arguments,
call id) is already the cell's `PendingCall`, pushed by the park arm before
the variant returns (`gate.rs:224-232, 251`); the wrapper needs no second
copy. Rejected: `Parked { call_id, tool_name }` — duplicates the cell record
(two sources of truth for one identity). The park arm's registration
sequence is UNCHANGED; only the return at `gate.rs:258-260` changes
(`ShortCircuit { sentinel }` → `Parked`). Scope: the DENIAL feedback
(`gate.rs:353-360`) and the recorded-denial consult (`gate.rs:386`) keep
`ShortCircuit` — a denial is a genuine decision outcome the model must see;
R7 targets only the park arm's fabrication.

**The crossing, wrapper → tool-server → rig loop.**

- Wrapper (`tool_wrapper.rs:523-570` gains the arm): on `Parked` — no
  `transform_output`, no `on_complete(Ok(…))`. The fabricated success event
  the deny leg showed (`deny-leg.sse:102-103`) is exactly what this removes;
  the parked call's client surface is the requested/pending pair
  (`gate.rs:235-249`). The wrapper crosses to the tool-server boundary as
  `Err(ToolError::Parked)`.
- `ComposedWrapper::pre_call` (`tool_wrapper.rs:763-786`): `Parked`
  short-circuits the chain exactly as `ShortCircuit` does today
  (`tool_wrapper.rs:785`) — later wrappers do not run.
- **The rig loop — the one named fork change.** The tool server's
  `call_tool` is `Result<String, ToolServerError>` and every `Err` renders
  as model-visible text today (`streaming.rs:367-374`) — there is no
  result-free channel. The fork (ours, pinned) gains the minimal tri-state:
  `ToolError` gains a `Parked` unit variant, and the loop's tool arm
  matches it BEFORE the `e.to_string()` fallback. The arm — an error arm
  that is not an error: (1) push the assistant `ToolCall` bookkeeping
  (`streaming.rs:389-391` — the pairing needs the call in the captured
  history); (2) produce NO tool result (skip the result push at :392 and
  the `ToolResult` yield at :397-401); (3) skip `on_tool_result` (no
  result to report); (4) continue the batch. The turn then ends normally
  and the next iteration's hook suspends the loop (decision 3).

Rejected alternatives: keep `Ok(PARK_SENTINEL)` crossing and suspend the
loop only — it keeps the fabricated result crossing the tool-server
boundary, violating R7's "stops returning the fake result"; an `Ok("")`
secret marker — a model-visible lie on any consumer that misses the
convention; a fork-level `break 'outer` on the park — puts park state in the
fork (the hook is the park authority) and skips the capture point.

Persistence consequence, ruled: no `on_complete` means no tool-trace row
for a parked call; the continuation prompt's tool chain will not list it.
Rejected: synthesize a trace row — it would record an execution that never
happened.

### 2. Sibling registration

The `Parked` control never touches `cancel_sig`. The fork's between-tool
checks (`streaming.rs:357-362, 382-387`) read the hard flag only, so the
batch drains exactly as today: gated siblings register + park, ungated
siblings execute normally. Mike's ruling holds with no extra mechanism —
the code is coherent with it (premise (a)). Rejected: abort the batch at
the first park — violates the sibling ruling and drops later calls'
assistant-turn bookkeeping (breaking the rebuild pairing).

### 3. The no-next-completion guarantee

Two named locks, belt and suspenders:

1. **The hook's park branch goes hard** (`streaming_request_hook.rs:496-508`):
   `cancel()` — the flag `is_cancelled()` reads (`mod.rs:174-176`). The fork
   then yields `Err(prompt_cancelled(...))` at `streaming.rs:281-285`
   BEFORE `stream_completion` — no completion ever carries the batch's
   prompt. The yielded Err's reason text reads `<no reason given>`: the
   hook's `CancelSignal` is a clone whose reason `OnceLock` is clone-local,
   so no stamp the hook can make reaches the loop — the park is identified
   by the blocked cell, and the hook's own log line carries the reason.
   Premise correction, on the record: TODAY's park "cancel" is
   reason-only and inert (`mod.rs:178-182`); "hook cancellation as today"
   prevents nothing — the deny leg's pivot is the proof.
2. **The consumer's first-Err return** (`orchestrator.rs:1524`): even if a
   future consumer pulled past the yield, the stream object is dropped and
   the generator never resumes into `stream_completion`.

Supporting ruling: the decision-ready short-circuit
(`orchestrator.rs:1543-1551`) is disarmed while the cell holds pending
parks (the `decision_ready` closure wired at `orchestrator.rs:3920-3923`
consults the cell) — a same-batch `submit_result` cannot end the stream
before the capture fires. Rejected: let a park+submit batch land
`Orphaned` (sweep + fail) — a same-message submit is live-reachable model
behavior, and failing the task on it turns a steerable park into a
user-visible failure; today's cross-turn park+submit lands `Blocked`, and
the disarm preserves that outcome class.

The model NEVER reasons over a parked call's fake outcome again: no
completion follows the parking batch (lock 1), and the resume replaces
every slot with a real decision outcome (stage 3, wired).

### 4. The capture point and snapshot shape

**The capture point is UNCHANGED**: the hook's `on_completion_call` park
branch, which still fires exactly once — at the completion attempt
FOLLOWING the parking batch (the fork runs the hook before the cancel
check, `streaming.rs:276-286`). `snapshot_if_pending` stays first-capture
(`types.rs:602-619`). Rejected: capture at stream end or cell drain inside
`execute_task` — the orchestrator never sees the conversation; the rig
stream owns it, and only the hook is handed `(history, prompt)`.

**The captured prompt is now synthesized** (the brief's preferred shape —
the sentinel becomes capture-side bookkeeping):

- Mixed batch (parked + executed siblings): `current_prompt` := the
  fork-built aggregated user message's items verbatim (the siblings' real
  results), then one sentinel `ToolResult` per pending call appended in
  cell order, each keyed by the pending call's OWN call id.
- All-parked batch: the fork aggregates no user message (no results,
  `streaming.rs:530`) and pops the assistant turn into `current_prompt`
  (`streaming.rs:538-541`); the hook reconstructs — history := history +
  [that assistant turn], `current_prompt` := the synthesized user message.

Either way `current_prompt` is a User message carrying at least one tool
result — the stage 2 witness (`ToolResultPrompt::try_new`,
REBUILD-DESIGN.md:60, 92-104) holds, and the sentinel literal is the
JSON-quoted `PARK_SENTINEL` wire form the calibration frame grounds
(`GOLDENS.md:73, 97-102`). **No stage 2 signature change.** Rejected:
fork-side synthesis — pushes sentinel knowledge into the fork; interleaving
slots in batch document order — the cell lacks the executed siblings'
positions, and the builder pairs by id, not position (the extras trust
boundary, REBUILD-DESIGN.md:150-157, makes the producer's slot order the
producer's own).

### 5. The retry-loop interaction

The park outcome is terminal for the attempt through the EXISTING arm: the
cell read (`orchestrator.rs:3929-3972`) runs BEFORE the result handling —
its comment already names "the park cancel's Err included" (`:3929-3930`)
— and the `CellOutcome::Blocked` return (`orchestrator.rs:3945-3949`)
precedes the no-`submit_result` retry arm (`orchestrator.rs:4075-4083`,
`None => { …; continue; }`). Under the new mechanism the stream lands as
`Err(prompt_cancelled …)` — reason text `<no reason given>`, per the
clone-local reason semantics above — the same terminal shape the arm
already handles, identified by the cell, not the Err's text. The load-bearing rule for stage 4b: the cell-read-first ordering
survives; a `Blocked` cell can never reach the retry arm. Rejected:
normalize the park `Err` into an Ok fall-through — it would reach
`None => continue` and re-drive a `Blocked` worker with a correction
prompt: the R7 pivot in new clothes.

### 6. Backward compat

Verified against REBUILD-DESIGN.md: the builder keys on call ids only,
never on sentinel text, and replaces-or-appends, accepting both producers
(`REBUILD-DESIGN.md:133-141`); the witness requires a User prompt carrying
at least one tool result (`REBUILD-DESIGN.md:92-104`). Today's producer's
documents — the same-message one-per-slot shape AND the pivot's
one-slot-total shape (the deny-leg document) — satisfy it; the new
producer's synthesized captures satisfy it. The stage 1 pivot fixtures
stage the legacy one-slot shape and stay green — they become the
backward-compat pins (frame vii). The stage 4b golden manifest row to add:
"legacy one-slot documents rebuild identically under the new producer —
pinned by the three pivot frames, unedited."

### 7. The frame plan (stage 4b)

**New frames** (producer assertions, driven through the worker-override
seam — `install_worker_overrides` + `override_park_orchestrator`, the
production path per `test_rig.rs:20-31`; the scripted model's request log
`test_rig.rs:198-218` is the model-visibility oracle):

| Frame | Pin |
| --- | --- |
| (i) `parked_call_produces_no_model_visible_tool_result` | Script [turn 1: gated call]. `model.requests().len() == 1`; no serialized request contains the sentinel; no `tool_call_completed(success)` for the parked id on the event surface; the inner tool never invoked. |
| (ii) `one_message_two_gated_calls_both_register_both_park_and_snapshot_captures` | Script [turn 1: gated A, gated B]. Two durable registrations (distinct decision ids); `TaskOutcome::Blocked`; pending [A, B]; snapshot captured; requests == 1. |
| (iii) `gated_sibling_after_a_park_registers_and_ungated_sibling_executes` | Script [turn 1: gated A, gated B, ungated C]. A and B registered; C executed exactly once; the captured `current_prompt` = [C's real result, sentinel slot A, sentinel slot B] — the merge order pinned; requests == 1. |
| (iv) request-count pins | Ride (i)-(iii): `requests().len() == 1`. Plus the negative pin: a one-turn script ends `Blocked`, NOT script-exhaustion — today's producer issues the second completion and faults the exhausted script; this pin is the re-drive's tombstone. |
| (v) `captured_prompt_carries_one_sentinel_slot_per_parked_call` | Pin the snapshot's `current_prompt` exactly: a User message, one sentinel `ToolResult` per pending call keyed by its own id, JSON-quoted wire form, siblings' real results verbatim. The pivot shape is structurally impossible: no producer frame can stage a re-driven-with-sentinel turn (request count 1; the producer emits no turn 2). |
| (vi) `attempt_loop_does_not_retry_on_blocked` | `MAX_WORKER_ATTEMPTS = 2` (`orchestrator.rs:97`); script [turn 1: gated call]; `TaskOutcome::Blocked` with attempt 1; exactly one worker build (one override consumed; one request in the log). |

**Existing frames that change** — assertion updates only, no fixture
staging changes:

- The gate park-arm unit frames pin today's outcome and flip to `Parked`:
  `happy_path_registers_publishes_appends_and_short_circuits`
  (`gate.rs:603`, assertion `:621-627`; rename with the outcome),
  `two_gated_calls_append_two_cell_entries` (`gate.rs:683`, `:702-703`),
  `webhook_poll_route_parks_the_gated_call` (`gate.rs:789`, `:829-835`).
- `tool_wrapper.rs`'s compose/pre-call unit frames gain the `Parked`
  passthrough pin (`tool_wrapper.rs:1179-1197`); the `ShortCircuitPreCall`
  frame (`tool_wrapper.rs:1044-1086`) stays — `ShortCircuit` still serves
  denials. `approver_headers.rs:113`'s manual match completes
  mechanically.

**Frames that stay byte-stable:**

- The orphan pair (`orphan_depth_exhaustion…` `orchestrator.rs:9177`,
  `orphan_provider_stream_error…` `:9232`): their staging still lands
  `Orphaned` — the depth check fires before the hook
  (`streaming.rs:229-233`), the mid-turn error break precedes bookkeeping
  (`:461-464`); the fail-closed sweep contract is unchanged.
- `park_commit_round_trips_the_replan_state_over_disk`
  (`orchestrator.rs:9369`): its pins don't touch the re-drive; today the
  exhausted script ends the stream after the inert cancel, tomorrow the
  park `Err` does — same `Blocked` outcome, no pinned byte moves.
- The full goldens.rs resume suite, the web handler frames, and both
  wire-calibration frames (`GOLDENS.md:73` — the sentinel literal
  survives as the synthesized slot's content).
- The three stage 1 pivot frames over `pivot_two_call_document`
  (`GOLDENS.md:65-67`) — now doubling as the legacy-snapshot
  backward-compat pins (frame vii); and the duplicate-fixture trio over
  `duplicate_key_document` (`GOLDENS.md:60-62`) — the fixture is the
  same-completion shape the new producer captures natively.
- `test_rig.rs`'s smoke frames (ungated).

## Premise corrections (recorded, none STOP-worthy)

1. **Today's park cancel is inert** — `cancel_with_reason` sets only the
   reason; `is_cancelled()` reads only the flag (`mod.rs:178-186`). The
   brief's candidate mechanism "hook cancellation as today" is a no-op;
   the spec makes the cancel hard (decision 3). This is also the entire
   resolution of the brief's stated contradiction — the re-drive is the
   fork's own next iteration, same loop, same conversation.
2. **"One-slot → one-per-call" sharpens**: today's same-message multi-park
   ALREADY captures one slot per call (both sentinels aggregate before the
   single capture); the one-slot-total shape arises only from cross-turn
   pivot parks, which the inert cancel enabled. The new producer eliminates
   cross-turn parks, making one-per-call the only multi-call capture. No
   existing frame drives a real two-call park (the Gate M README's own
   finding), so no fixture restaging is forced.
3. **Cross-repo dependency, reported**: a result-free crossing requires a
   rig-fork change — the tool server's `Result<String, ToolServerError>`
   has no third state and renders every `Err` as model-visible text
   (`streaming.rs:367-374`). The spec's fork change is minimal and named
   (decision 1); stage 4b owns the fork pin bump. Not a STOP — the brief's
   own item 1 asks what the rig loop does with the outcome, which is
   loop-level design authority.
4. **REBUILD-DESIGN.md's binding condition holds**: the synthesized capture
   keeps `current_prompt` a tool-result user message (decision 4); the
   witness and builder need no change; no stage 2 signature change arises.

No STOP-and-report items: no mapped fact contradicts the R7 ruling, the
brief's frame obligations, or REBUILD-DESIGN.md's premises. The one
behavior the mechanism retires — the cross-turn pivot park — is the
behavior R7 exists to retire.
