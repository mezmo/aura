# P45 stage 2 design record — the provider-valid context builder (R5)

Layer 1 typed-holes unit for the reconstruction direction: real
signatures, real derives, `todo!()` bodies for the three functions with
behavior. The type surface lives in `orchestration/park/rebuild.rs` and
is re-exported flat from the park module; nothing consumes it yet —
P45 stage 2b fills the bodies (its unit frames pin the behavior), and
P45 stage 3 points the substitution prelude at the builder, retiring
`continuation::replace_tool_result` there. Sources: the amended
reify-flow plan (stage 2), the ruled sketch's frontier review (R5), and
the Stage 1 pivot frames in `park/resume/goldens.rs`
(`pivot_two_call_document` and `assert_paired_outcome`), which are the
spec this surface must serve.

## The two construction phases (wire timing)

The bundle cannot be built in one step, because its two inputs exist at
different times: the node's pending calls are on the checkpoint the
evaluation read, but an outcome wire exists only after the segment
invokes the decided call (an approved call's wire is its execution
result; a denial short-circuits without one). The refusal the plan
demands — an empty `call_id` must refuse the segment BEFORE any
invocation, not per-node (the gate's park arm can stamp an empty id when
the stream hook observed no tool-call id, and a per-node preflight would
let node A execute before node B's empty id surfaces) — therefore
attaches to the CALL side, parsed before any invocation:

1. **Preflight.** The prelude runs `ValidatedCalls::try_new` for every
   awaiting node before any tombstone or invocation. This constructor is
   the bundle's fallible constructor: validation lives here (empty
   list, any member's empty call id, a shared id), and the error enum is
   complete in the unit.
2. **Resolution.** After a node's invocations produce its wires,
   `ValidatedCalls::resolve` pairs each call with the i-th outcome,
   positionally in document order — wrong pairing order is
   unrepresentable by construction, not checked at pair time. A count
   other than one-per-call is the only fault left, a wiring-bug guard.

`ResolvedCallBundle` is constructible by no path but `resolve`, so
`rebuild_context`'s input is valid by construction: there is no type for
a half-resolved or mispaired bundle.

## Type → business rule → forbidden invalid state

| Type | Business rule | Invalid state made unrepresentable |
| --- | --- | --- |
| `CallId` | A rebuilt tool result and its assistant call key on the pending call's own id; the gate's park arm can stamp an empty id, and an empty id keys nothing | An empty (hence unkeyable) call id reaching any reconstruction slot |
| `OutcomeWire` | The outcome a decided call's tool result carries is the chain's own wire rendering (Ok JSON-quoted, execution Err raw, denial the live text); the builder carries it verbatim | A re-rendered, double-encoded outcome; a bare string travelling where the wire rendering rule applies |
| `ValidatedCall` | One bundle member is the checkpoint's own record (decision, tool, arguments) with the id validated; tool and arguments ride verbatim — the consult's mismatch row already refuses a record that diverges from the approval the human saw | A member whose id was never checked; a member missing its decision linkage |
| `ValidatedCalls` | The bundle is per awaiting node, ordered in document order, non-empty, ids unique within the node; the segment-wide preflight parses every node's list before any invocation | A bundle for a node that never parked calls; node A executing before node B's empty id surfaces; ambiguous id-keyed pairing |
| `ResolvedCall` | Each call travels paired with its own outcome wire — pairing is construction, not an operation on two parallel lists | A call and an outcome pairing in the wrong order; an outcome applicable to another call's slot |
| `ResolvedCallBundle` | The builder's input is valid by construction | Handing the builder a mispaired, half-resolved, or empty bundle |
| `RebuildError` | Why a bundle could not be built or a context rebuilt; payloads are `Diagnostic`s no caller branches on | A caller branching on error prose; a raw string crossing the boundary |
| `RebuiltContext` | Every tool result in the combined context is preceded by the assistant tool call keyed to the same id; the captured history is otherwise byte-preserved | An orphaned tool result (providers reject it); a silently restructured turn boundary |
| `rebuild_context` | Per-node reconstruction from the snapshot plus the resolved bundle, pure; total over its inputs except the one named second failure mode | A rebuild that consults the store, the clock, or anything but its two inputs |

## The builder's second failure mode (named for the panel)

The dispatch's default is a total builder; this unit claims the one
genuine exception. The bundle is validated, but the SNAPSHOT is not:
nothing between the deserialized checkpoint document and the builder
checks the prompt's shape. Both park producers — today's sentinel
producer and the stage 4 parked-control-outcome producer (whose
mechanism spec is still owed) — aggregate tool results into a USER
message; rig's `Message` is two-variant, so "not the tool-result prompt"
is exactly "an Assistant prompt". A non-user `current_prompt` is
reachable only through a malformed or tampered document, and the builder
refuses it (`RebuildError::NotAToolResultPrompt`) rather than
restructuring — moving a non-user prompt into the history would silently
reshape the context that stage 3's turn-boundary rework slices, papering
over a producer or document violation. The alternative — a validated
borrow wrapper (`ToolResultPrompt<'a>` constructed fallibly at the call
site, builder total) — has the same observable and moves the check to
the caller; it was rejected for one-call simplicity, and is the first
panel question below.

## Visibility / seam table

| Seam | Visibility | Consumer |
| --- | --- | --- |
| `park/mod.rs` re-export block | `#[allow(unused_imports)] pub(crate) use rebuild::{…}` — every type named in a re-exported signature, per the panel-ruled re-export completeness | P45 stage 3's prelude (`orchestrator.rs`); the marker sweeps at wiring |
| `ValidatedCalls::try_new`, `ValidatedCalls::resolve`, `rebuild_context` | `pub(crate)` | the prelude; the stage 2b frames |
| `OutcomeWire::new`; `ValidatedCall` accessors; `as_slice` on all three bundle types; `RebuiltContext::history`/`current_prompt`/`into_parts` | `pub(crate)` | the prelude; the stage 2b frames (`into_parts` feeds the continuation stream call in the stage 3 parameter order: prompt, then history) |
| `CallId`, `ValidatedCall`, `ResolvedCall` construction | none — module-internal, after validation | stage 2b bodies only |
| `RebuildError` → segment fault | `Display` only; no `From` into `SegmentError` in this unit | the prelude renders `RebuildError` into `SegmentError::Continuation` via its `Diagnostic` |
| `#![allow(dead_code)]` on the module | sweep with the re-export marker at wiring | — |

## Residual risks

- **Both producers must stay accepted (ruled risk).** The builder keys on
  call ids only — never on sentinel text — and replaces-or-appends, so
  it accepts today's sentinel-slot snapshots AND the stage 4 producer's
  control-boundary snapshots. The binding condition: stage 4's mechanism
  spec must keep `current_prompt` a tool-result user message (the
  sentinel slot may vanish, the message shape may not), or the named
  second failure mode fires as designed — loudly, not silently.
- **The provider-ordering assumption (ruled risk).** Every provider the
  chain speaks requires a tool result to FOLLOW its own assistant tool
  call; the builder guarantees it by construction — synthesized calls
  append AFTER the snapshot's captured messages (document order among
  themselves), and every appended result follows its call. Reused
  captured calls are never moved. A provider that rejected a
  same-turn-later ordering would need a different builder; none is known.
- **Extra non-bundle results are preserved, not dropped.** A re-park's
  fresh snapshot carries the prior resume's outcomes as tool results for
  ids outside the new bundle (the A2 lifecycle shape). The builder keeps
  them verbatim — they are model-visible truth — so the goldens'
  "no extras" pins hold for the single-resume fixtures by fixture shape,
  not by builder dropping.
- **Duplicate sentinel slots for one bundle id are collapsed.** The
  producers write one slot per parked call; a document with two slots
  for one id is malformed but not refused — the 2b rule is
  first-slot-replaced, later-duplicates-collapsed, holding the
  "exactly one result per call" invariant.
- **Empty `tool_name` is not validated here.** Both producers stamp it
  from the registered tool's definition (`ctx.tool_name`, gate.rs) —
  and the wrapper chain resolves a tool BY name before the gate ever
  sees the call, so an empty name has no upstream path. The consult's
  mismatch row cannot catch emptiness (the store's approval item and
  the document's pending call carry the same `ctx.tool_name`, so an
  empty name would agree, not diverge). If the pipeline guarantee ever
  broke, the builder would synthesize an assistant tool call with an
  empty function name; the panel may prefer an explicit refusal
  (question 6).
- **`OutcomeCountMismatch` is a wiring-bug guard**, unreachable when the
  prelude invokes each call exactly once and collects in document order.
  If it ever fires, the segment drive has a pairing defect; stop and
  fix the drive, not the count check.

## Hole inventory (`todo!()` over the unit)

| Location | Hole |
| --- | --- |
| `park/rebuild.rs` | `ValidatedCalls::try_new` — the validations: empty list, per-member empty call id, duplicate ids; members built from the calls' own records |
| `park/rebuild.rs` | `ValidatedCalls::resolve` — positional pairing, one-per-call count check |
| `park/rebuild.rs` | `rebuild_context` — the reconstruction: per-call presence check and synthesis in the history, replace-or-append (with duplicate-slot collapse) in the prompt, byte-preservation elsewhere |

Trivial accessors, constructors of validated newtypes, and
`RebuildError`'s `Display` are implemented per the skeleton-unit rules;
the three holes above are the whole behavior surface stage 2b owns.

## Questions for the design panel

1. **Two-phase construction vs one fallible constructor over paired
   members.** The wire-timing argument forces the call-side validation
   ahead of invocations; the alternative (a single constructor taking
   calls + wires, called after invocations) cannot refuse an empty call
   id segment-wide before any tool runs. Is the phase split the right
   cost, or should the preflight validation live outside the bundle (a
   `ValidatedCalls`-shaped type owned by the prelude) with the bundle
   single-phase?
2. **The builder's second failure mode.** Refuse a non-tool-result
   prompt (chosen), restructure total-with-move (rejected — silent turn
   reshaping), or move the check into a validated borrow wrapper so the
   builder's signature is total? Same observable in all three; which
   home does the panel want?
3. **Preserve-extras reading.** Confirm the preserve-verbatim rule for
   non-bundle tool results against the A2 two-resume lifecycle frames —
   the alternative (dropping ids outside the bundle) would delete a
   prior resume's genuine outcomes from a re-parked context.
4. **`OutcomeWire` as a newtype vs a bare `String` on the member.** The
   newtype's rule is "pre-rendered, carried verbatim, never re-rendered"
   — the double-encoding guard. Is the wrapper earning its width?
5. **`Diagnostic` payloads on `RebuildError`** (the rule-5 escape hatch,
   mirroring `RehydrateError`'s planned wrapping): the stage 2b frames
   will pin the wordings; confirm no caller should branch on the
   variants themselves beyond rendering.
6. **Empty `tool_name`:** refuse it in `try_new` (a fourth validation,
   variant `EmptyToolName`), or trust the invocation pipeline's
   name-lookup guarantee as recorded in the residual risks? The unit
   trusts the guarantee; an explicit refusal is defensible if the panel
   wants the synthesis surface closed by construction.
