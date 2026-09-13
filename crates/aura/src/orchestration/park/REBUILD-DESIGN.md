# P45 stage 2 design record — the provider-valid context builder (R5)

Layer 1 typed-holes unit for the reconstruction direction: real
signatures, real derives, `todo!()` bodies for the five functions with
behavior. The type surface lives in `orchestration/park/rebuild.rs` and
is re-exported flat from the park module; nothing consumes it yet —
P45 stage 2b fills the bodies (its unit frames pin the behavior), and
P45 stage 3 points the substitution prelude at the builder, retiring
`continuation::replace_tool_result` there. Sources: the amended
reify-flow plan (stage 2), the ruled sketch's frontier review (R5), and
the Stage 1 pivot frames in `park/resume/goldens.rs`
(`pivot_two_call_document` and `assert_paired_outcome`), which are the
spec this surface must serve. This revision folds the two-seat design
panel's FAIL verdict (ledger at the end): keyed pairing, the preflight
prompt witness, the segment-level door, the split error surfaces, and
the scoped invariant are the panel's repairs.

## The two construction phases (wire timing)

The bundle cannot be built in one step, because its two inputs exist at
different times: the node's pending calls are on the checkpoint the
evaluation read, but an outcome wire exists only after the segment
invokes the decided call (an approved call's wire is its execution
result; a denial short-circuits without one). Both refusals the plan
demands therefore attach to the PREFLIGHT, which runs before any
tombstone or invocation:

1. **Preflight, segment-wide.** `SegmentPreflight::try_new` takes every
   awaiting node's input (`NodePreflightInput`: the pending-call list
   and the snapshot prompt) and validates them together — per node, the
   call checks (empty list, empty call id, empty tool name, duplicate
   ids) plus the prompt-shape check (`ToolResultPrompt::try_new`). The
   constructor yields the per-node validated halves only when EVERY
   node validated; the first fault names its node and refuses the whole
   segment. `ValidatedCalls::try_new` is private to the module, so the
   segment constructor is the only door — no bundle exists for any node
   unless every node validated, literally by construction.
2. **Resolution, keyed.** After a node's invocations produce its wires,
   `ValidatedCalls::resolve` pairs each call with the outcome keyed to
   the call's OWN id — pairing by identity, not position. An outcome
   keyed outside the bundle, a key used twice, and a call left without
   its outcome are all refused; the surviving caller obligation is one
   outcome per bundle call, keyed by the validated calls' own ids.

`ResolvedCallBundle` is constructible by no path but `resolve`, and
`rebuild_context` consumes the bundle plus the prompt witness — so the
builder is TOTAL: every failure mode lives at the preflight or the
resolution, both before the continuation streams and both before any
invocation can be half-spent.

## Type → business rule → forbidden invalid state

| Type | Business rule | Invalid state made unrepresentable |
| --- | --- | --- |
| `CallId` | A rebuilt tool result, its assistant call, and its outcome wire all key on the pending call's own id; the gate's park arm can stamp an empty id, and an empty id keys nothing | An empty (hence unkeyable) call id reaching any reconstruction slot or pairing key |
| `OutcomeWire` | The outcome a decided call's tool result carries is the chain's own wire rendering; `OutcomeWire::new` is the only door onto it | A bare `String` reaching resolution without passing the named constructor (the no-double-encoding rule itself travels by convention and audit, not by the type) |
| `ValidatedCall` | One bundle member is the checkpoint's own record (decision, tool, arguments) with the id and tool name validated | A member whose id was never checked; an empty tool name reaching synthesis |
| `ValidatedCalls` | The per-node call side: ordered in document order, non-empty, ids unique, tool names non-empty; a transition state consumed by `resolve` | A bundle for a node that never parked calls; same-node duplicate ids; a cloned transition state resolving twice |
| `NodePreflightInput` | One node's preflight input pairs its call list with its own prompt | A prompt attributed to another node's calls |
| `ToolResultPrompt` | The prompt witness: the snapshot prompt validated as the tool-result user message — the only shape the rebuild can carry outcomes in — validated at preflight, where it is knowable before any tombstone or invocation | A non-tool-result prompt reaching the builder; the shape check hiding inside the builder as a runtime failure |
| `ValidatedNode` | The per-node half: validated calls plus the prompt witness, built only when every node validated | A half surviving another node's refusal |
| `SegmentPreflight` | The segment-level door: every awaiting node validated together, or nothing | A bundle existing for node A while node B's invalid input has not surfaced — the per-node preflight's hazard |
| `ResolvedCall` | Pairing by identity: each call travels with the outcome keyed to its own id — resolution pairs, callers cannot | A call and an outcome pairing in the wrong order; parallel-list mispairing |
| `ResolvedCallBundle` | The builder's input is valid by construction; constructible only through `resolve` | Handing the builder a mispaired or half-resolved bundle |
| `PreflightError` / `ResolveError` | Why the preflight or the resolution refused; payloads are `Diagnostic`s no caller branches on | A caller branching on error prose; a raw string crossing the boundary |
| `RebuiltContext` | Every BUNDLE-KEYED tool result is preceded by the assistant tool call of the same id; the captured history is otherwise byte-preserved, extras included; non-bundle extras inherit the producer's pairing (the trust boundary) | An orphaned BUNDLE result; a silently restructured turn boundary |
| `rebuild_context` | Total, pure reconstruction from three validated-by-construction inputs; no failure mode of its own | A rebuild that can fail after the preflight cleared; a rebuild consulting anything but its inputs |

## The prompt-shape witness (the former "second failure mode")

The original skeleton kept one failure mode inside the builder: a
snapshot prompt that is not the tool-result user message (rig's
`Message` is two-variant, so this is exactly "an Assistant prompt",
reachable only through a malformed checkpoint document — both park
producers, today's sentinel producer and the stage 4
parked-control-outcome producer, aggregate tool results into a USER
message). The panel's repair hoists the check to the preflight, where
it is knowable before any tombstone or invocation:
`ToolResultPrompt::try_new` is the fallible witness constructor, its
refusal (`PreflightError::NotAToolResultPrompt`) fires before any tool
runs, and `rebuild_context` requires the witness and is therefore
total. The rejected alternatives, recorded: total-with-restructure
(moving a non-user prompt into the history would silently reshape the
context the stage 3 turn-boundary rework slices) and the broader
fallible validated-snapshot of seat 1's finding S1-2 (rejected — it
adds a malformed-document-only failure mode for no production gain; the
scoping of the rebuild invariant to bundle-keyed results was chosen
instead). The binding condition on stage 4's mechanism spec stands:
keep `current_prompt` a tool-result user message, or the preflight
refuses — loudly, before any invocation.

## Stage 2b frame obligations (from the repair round)

- **One-turn synthesis.** Where the park missed more than one of a
  node's calls, the synthesized calls append as ONE assistant turn
  carrying all of them — the producer's same-completion shape, which
  genuine sibling captures also produce. A stage 2b frame must pin this
  (an N>1 synthesis frame), not just per-call presence.
- **Keyed-pairing caller obligation.** `resolve` refuses unknown ids,
  duplicate keys, and calls left without outcomes; the surviving caller
  obligation is exactly one outcome per bundle call, keyed by the
  validated calls' own ids (taken from `ValidatedCall::call_id` before
  `resolve` consumes the list).

## Visibility / seam table

| Seam | Visibility | Consumer |
| --- | --- | --- |
| `park/mod.rs` re-export block | `#[allow(unused_imports)] pub(crate) use rebuild::{…}` — every type THIS UNIT introduces and names in a `pub(crate)` signature. `Diagnostic` is deliberately absent: it is `park::resume`'s type, already re-exported through resume's block and `orchestration`, and re-exporting it here would give it duplicate provenance; the completeness claim is scoped accordingly rather than the re-export widened | P45 stage 3's prelude (`orchestrator.rs`); the marker sweeps at wiring |
| `ValidatedCalls::try_new` | module-PRIVATE — the segment constructor is the only door | stage 2b bodies only |
| `ToolResultPrompt::try_new`, `SegmentPreflight::try_new`, `ValidatedCalls::resolve`, `rebuild_context` | `pub(crate)` | the prelude; the stage 2b frames |
| `as_slice` | on the three types that have it: `ValidatedCalls`, `ResolvedCallBundle`, `SegmentPreflight` | inspection (frames and prelude) |
| `NodePreflightInput::new`; `ValidatedNode::calls`/`prompt`/`into_parts`; `ValidatedCall` accessors; `ResolvedCall::call`/`wire`; `RebuiltContext::history`/`current_prompt`/`into_parts`; `OutcomeWire::new`; `AsRef` on `CallId`/`OutcomeWire`/`ToolResultPrompt` | `pub(crate)` | the prelude; the stage 2b frames (`into_parts` feeds the continuation stream call in the stage 3 parameter order: prompt, then history) |
| `PreflightError`/`ResolveError` → segment fault | `Display` only; no `From` into `SegmentError` in this unit | the prelude renders them into `SegmentError::Continuation` via their `Diagnostic`s |
| `#![allow(dead_code)]` on the module | sweep with the re-export marker at wiring | — |

## Residual risks

- **Both producers must stay accepted (ruled risk).** The builder keys
  on call ids only — never on sentinel text — and replaces-or-appends,
  so it accepts today's sentinel-slot snapshots AND the stage 4
  producer's control-boundary snapshots. The binding condition: stage
  4's mechanism spec must keep `current_prompt` a tool-result user
  message, or the preflight refuses (`NotAToolResultPrompt`) — loudly,
  and since the hoist, before any tombstone or invocation.
- **The provider-ordering assumption (ruled risk).** Every provider the
  chain speaks requires a tool result to FOLLOW its own assistant tool
  call; the builder guarantees it by construction for bundle-keyed
  results — synthesized calls append AFTER the snapshot's captured
  messages (one assistant turn for all of a node's missing calls), and
  every appended result follows its call. Reused captured calls are
  never moved. A provider that rejected a same-turn-later ordering
  would need a different builder; none is known.
- **The extras trust boundary (corrected, S2-3).** A prior resume's
  outcomes riding a re-parked snapshot live in that snapshot's HISTORY
  — byte-preservation covers them, and the rebuild invariant does not
  re-verify their pairing. Tool results carried by the PROMPT for ids
  outside the bundle are the stage 4 producer's shape (today's producer
  writes prompt slots only for parked calls); they are preserved
  verbatim, their pairing the producer's. The goldens' "no extras" pins
  hold by fixture shape, not by the builder dropping anything.
- **Duplicate sentinel slots for one bundle id are collapsed.** The
  producers write one slot per parked call; a document with two slots
  for one id is malformed but not refused — the 2b rule is
  first-slot-replaced, later-duplicates-collapsed, holding the
  "exactly one result per call" invariant.
- **The resolution faults are wiring-bug guards.** `UnknownOutcomeId`,
  `DuplicateOutcomeId`, and `MissingOutcome` (the keyed pairing's
  decomposition of the earlier count-mismatch fault) are unreachable
  when the prelude invokes each call exactly once and keys its outcomes
  by the validated calls' own ids. If one ever fires, the segment drive
  has a pairing defect; stop and fix the drive, not the check.
- **Empty `tool_name` is refused at the preflight.** The invocation
  pipeline's name-lookup guarantee (a tool is resolved BY name before
  the gate ever sees the call) stands as defense-in-depth context; the
  preflight refusal now closes the synthesis surface by construction
  regardless.

## Hole inventory (`todo!()` over the unit)

| Location | Hole |
| --- | --- |
| `park/rebuild.rs` | `ValidatedCalls::try_new` — the call checks: empty list, per-member empty call id, per-member empty tool name, duplicate ids |
| `park/rebuild.rs` | `ToolResultPrompt::try_new` — the prompt-shape check |
| `park/rebuild.rs` | `SegmentPreflight::try_new` — the all-or-nothing loop over every node's input |
| `park/rebuild.rs` | `ValidatedCalls::resolve` — keyed pairing: unknown id, duplicate key, missing outcome |
| `park/rebuild.rs` | `rebuild_context` — the reconstruction: per-call presence check, one-turn synthesis, replace-or-append with duplicate-slot collapse, byte-preservation |

Trivial accessors, `NodePreflightInput::new`, `ValidatedNode::into_parts`,
`SegmentPreflight::into_nodes`, `RebuiltContext::into_parts`, and both
`Display` impls are implemented per the skeleton-unit rules; the five
holes above are the whole behavior surface stage 2b owns.

## Panel ledger

Two-seat design panel over the stage-2a skeleton (commit b9b18570):
seat 1 `rust-reviewer` (openai/gpt-5.6-sol), seat 2
`frontier-reviewer` (Kimi-K3); author GLM-5.3-Fast (both seats differ
from the author's family). Verdict FAIL on both seats — seat 1: 4
BLOCKING + 1 MINOR; seat 2: 2 BLOCKING + 4 MINOR. The board owner's
dispositions below are the repair contract this revision lands.

| Seat | Finding | Severity | Disposition | Repair |
| --- | --- | --- | --- | --- |
| S1 (rust-reviewer, gpt-5.6-sol) | S1-1: positional pairing in `resolve` is a latent mispairing hazard (iterator order, not identity, pairs calls with wires) | BLOCKING | ACCEPT — pairing by identity | 1: `resolve` takes `(CallId, OutcomeWire)`; unknown/duplicate/missing refusals; type-map row updated to identity |
| S1 | S1-2: the malformed-document surface (prompt shape, tool name) demands a validated-snapshot input | BLOCKING | Split — the fallible validated-snapshot sub-repair REJECTED (adds a malformed-document-only failure mode for no production gain); the scoping of the rebuild invariant to bundle-keyed results adopted instead. The prompt-shape and empty-tool-name checks DID land at preflight, via S2-1 and S1-4 | 6 (scoping) + 2 (witness) + 5 (tool name) |
| S1 | S1-3: one `RebuildError` spans two operations (construction and resolution), and the builder carries an error type it should not have | BLOCKING | ACCEPT — split per operation; builder total | 3: `PreflightError` / `ResolveError`; `rebuild_context` returns `RebuiltContext` directly |
| S1 | S1-4: an empty tool name would synthesize a provider-invalid assistant call | BLOCKING | ACCEPT in part — preflight refusal adopted; the validated `ToolName` threaded through production `PendingCall` REJECTED (stage scope) | 5: `PreflightError::EmptyToolName`, checked alongside `EmptyCallId` |
| S1 | S1-5: `Clone` on the transition-state `ValidatedCalls` lets a validated list resolve twice | MINOR | ACCEPT | 7: `Clone` removed |
| S2 (frontier-reviewer, Kimi-K3) | S2-1: the prompt-shape check inside the builder is a hoistable failure mode — validate at preflight (knowable before any tombstone/invocation) and make the builder total via a witness | BLOCKING | ACCEPT | 2: `ToolResultPrompt` witness, `NotAToolResultPrompt` to the preflight surface, `rebuild_context` total |
| S2 | S2-2: per-node validation lets node A execute before node B's invalid input surfaces — the refusal must be segment-level and all-or-nothing | BLOCKING | ACCEPT | 4: `SegmentPreflight` door; `ValidatedCalls::try_new` module-private |
| S2 | S2-3: the record misplaces the A2 lifecycle extras — prior outcomes ride the fresh snapshot's HISTORY (byte-preservation's job), not the prompt; prompt-carried extras are stage 4's shape | MINOR | ACCEPT | 6: invariant scoped to bundle-keyed results; trust boundary named; record corrected |
| S2 | S2-4: seam-table drift — the `as_slice` row over-claims; `Diagnostic` contradicts the re-export-completeness claim | MINOR | ACCEPT — `as_slice` row names its three types; the completeness claim reworded to this unit's own types (re-exporting `Diagnostic` rejected as duplicate provenance) | 8 |
| S2 | S2-5: the positional-pairing claim needs at least a wording fix; structurally, keyed pairing is the real repair | MINOR | ACCEPT — this seat's wording-only option SUPERSEDED by S1-1's keyed repair; the sub-finding (one-turn synthesis shape + honest caller-obligation statement) adopted | 1 + 10 |
| S2 | S2-6: the `OutcomeWire` type-map row over-claims — the type cannot enforce the no-double-encoding rule | MINOR | ACCEPT — reworded to the constructor-door half; the encoding rule travels by convention + audit | 9 |

Marker health after stage 2b: the five behavior holes
(`ValidatedCalls::try_new`, `ToolResultPrompt::try_new`,
`SegmentPreflight::try_new`, `ValidatedCalls::resolve`,
`rebuild_context`) are FILLED, their `#[expect(unused_variables)]`
markers swept by hand (`grep -n "expect(unused_variables"` over
`rebuild.rs` returns nothing; every filled body uses every parameter);
the module's `#![allow(dead_code)]` and the re-export's
`#[allow(unused_imports)]` keep their stage-3 sweep notes.

## Stage 2b coverage manifest (frames → surface)

Every frame lives in `rebuild.rs`'s test module; builder frames assert
the complete rebuilt `(history, current_prompt)` sequence as exact rig
messages — whole-context pins, not substring probes.

| Frame | Surface covered |
| --- | --- |
| `pivot_shape_appends_one_synthesized_turn_and_carries_both_results_in_document_order` | FRAME 1, the full happy path: one-turn synthesis from the pending call's own record (tool + arguments verbatim), sentinel slot replaced in place, missing result appended in document order, captured history byte-preserved, sentinel nowhere |
| `two_slot_shape_replaces_both_slots_in_place_and_keeps_the_history_byte_identical` | FRAME 2: both sentinel slots replaced in place; reused captured calls never move; history byte-identical; nothing synthesized |
| `every_rebuilt_tool_result_is_preceded_by_its_own_call_and_answers_once` | FRAME 3, the pairing invariant over both shapes: every tool result preceded by an assistant call keyed to the same id; ids issued once, answered exactly once |
| `three_call_pivot_groups_both_missing_calls_into_one_appended_turn_in_document_order` | FRAME 4, the panel's one-turn-synthesis obligation (S2-5): N>1 missing calls land in ONE appended assistant turn, in document order |
| `extras_outside_the_bundle_ride_verbatim_in_history_and_prompt` | FRAME 5, the extras trust boundary (S2-3): a prior resume's paired call+result rides the history byte-preserved; a non-bundle prompt result keeps the snapshot's position and bytes |
| `empty_call_id_refuses_the_segment_preflight` | FRAME 6a: `PreflightError::EmptyCallId`, wording pinned (`awaiting node 0: pending call member 0 carries an empty call id`) |
| `empty_tool_name_refuses_the_segment_preflight` | FRAME 6b: `PreflightError::EmptyToolName`, wording pinned — the S1-4 synthesis-surface closure |
| `duplicate_call_ids_refuse_the_segment_preflight` | FRAME 6c: `PreflightError::DuplicateCallId`, wording pinned (`awaiting node 0: two pending calls share the call id call_apply_1`) |
| `empty_pending_list_refuses_the_segment_preflight` | FRAME 6d: `PreflightError::EmptyCalls` (unreachable in the wired flow; the bundle's non-empty guarantee is structural) |
| `assistant_prompt_refuses_the_segment_preflight` | FRAME 6e: `PreflightError::NotAToolResultPrompt` — the S2-1 hoist, before any tombstone or invocation |
| `one_nodes_invalid_input_refuses_the_whole_segment_at_the_door` | FRAME 7, the segment door's all-or-nothing property (S2-2): node B's fault refuses the whole segment though node A alone validates; the diagnostic names the node (`awaiting node 1: …`) |
| `out_of_order_outcomes_still_pair_by_id` | FRAME 8a: keyed pairing (S1-1) — outcomes out of order still pair by identity; the bundle keeps document order |
| `outcome_keyed_outside_the_bundle_refuses` | FRAME 8b: `ResolveError::UnknownOutcomeId`, wording pinned |
| `duplicate_outcome_keys_refuse` | FRAME 8c: `ResolveError::DuplicateOutcomeId`, wording pinned |
| `call_left_without_its_outcome_refuses` | FRAME 8d: `ResolveError::MissingOutcome`, wording pinned (names the abandoned call and its tool) |
| `rebuilt_context_survives_the_rig_message_serialization_round_trip` | FRAME 9: provider-bound wire acceptance at the reachable seam — every rebuilt message JSON round-trips identical, sentinel-free (see the exclusion row) |

### Exclusions

| Exclusion | Reason | Owner |
| --- | --- | --- |
| Bedrock request-encode (frame 9's preferred form) | No reachable seam exists. The pinned rig fork (mezmo/rig @ 097d08d6) implements the encode — `TryFrom<RigMessage> for aws_bedrock::Message`, consumed by `CompletionRequest::messages()` — behind `pub(crate)` visibility in `rig_bedrock::types`, unreachable from aura; the only public path is `CompletionModel::completion`, which needs an AWS client and the network. aura itself carries no encode seam (`builder.rs`/`orchestrator.rs` construct rig agents; the encode happens inside the fork at request time), and no repo test round-trips rig Messages into a provider request shape. Per the fill-unit rules no fake seam was invented: the frame asserts the rig Message serde round-trip instead — the same serializer the resume goldens' wire-calibration frame (`outcome_pair_and_sentinel_literals_match_the_wire_serializers`) grounds its literals against. | The encode belongs to the rig fork; end-to-end provider acceptance lands with P45 stage 3's prelude wiring, which speaks through rig's Completion trait |
| Reverse-capture builder shape (a later bundle call captured while an earlier one is missed) | Unreachable from both producers: calls issue in turn order, and the snapshot captures a completion's assistant turn all-or-nothing, so a missed call is always later in document order than a captured one — replace-in-place plus append therefore always yields bundle document order. Pinned by construction reasoning, not a frame. | The stage-4 producer's mechanism spec carries the binding condition |
| The three Stage-1 pivot goldens stay red (`pivot_approved_pair…`, `pivot_approve_then_deny…`, `pivot_denied_pair…`, all on `call_scale_2`) | By design: they pin the WIRED substitution, which stage 3's prelude rework delivers; stage 2b fills and frames the builder only. Post-2b full-lib suite: 1299 passed (1283 + 16 frames), exactly these 3 red. | P45 stage 3 flips them |
| `#![allow(dead_code)]` on the module; `#[allow(unused_imports)]` on the park/mod.rs re-export | Survivors by design: the module stays unwired until stage 3 points the substitution prelude at it. | P45 stage 3 sweeps both at wiring |
