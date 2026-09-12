# P45 golden coverage manifest

Map of the ruled resume rows (2026-09-12, card P45) to their whole-frame
golden fixtures, with the exclusions the goldens do not cover. The frames
live in two suites, split along the visibility wall the DESIGN.md seam table
records:

- **Pipeline frames** —
  `orchestration::park::resume::goldens` (this directory, `goldens.rs`):
  drive `evaluate_resume` / `run_segment` over production-reachable
  fixtures (file-backed approval store, `publish`-ed documents, matching
  `config_fingerprint`), and pin the complete 409 body as the exact
  `serde_json` value of the `ResumeConflictRow` — the value axum's
  `Json(row)` serializes onto the wire.
- **Handler/projection frames** — `aura-web-server`
  `handlers::tests::resume_goldens`: drive `resume_run` directly with a
  constructed `AppState` + `ResumeClaims` extension, and pin the status +
  body artifacts the handler layer owns (bare 404s; the 200-body
  projection).

## Scope of the claim

Green goldens prove the wire artifacts (status + complete body values) are
unchanged. Downstream of the wire they are silent: side-effect ordering,
broker events, and restart behavior all stay out of scope, except where a
frame asserts an adjacent observable (row 6's rename-back, row 7's untouched
document and ticket). A green suite is not an endorsement of the fill's
internals.

## Row → fixture map

| Ruled row | Surface pinned | Fixture (suite) |
| --- | --- | --- |
| 1. malformed session or run id → 404, no detail, no read | bare 404 + no-read ordering | `malformed_session_id_answers_bare_404_without_reading_the_filesystem`, `malformed_run_id_answers_bare_404_without_reading_the_filesystem` (web). The memory root is a regular file: any read would fault, never 404. The structural no-read guarantee is `ValidatedResumePath` gating `ResumeDocuments::for_path`. |
| 2. no checkpoint under either name → 404, no detail | detail-less verdict + bare 404 | `missing_checkpoint_refuses_with_the_document_absent_row` (aura); `missing_checkpoint_answers_bare_404` (web). |
| 3. binding on, presented hash differs → 404, no detail | detail-less verdict | `identity_hash_mismatch_refuses_with_the_detail_less_row` (aura). The park commit writes the hashed presented header when binding is on (the orchestrator proof test `bound_park_stamps_the_identity_hash_and_resume_enforces_it` exercises the park-reachable state end to end). |
| 4. live in-process claim → 409 running, blocking [] | whole 409 body | `held_claim_refuses_the_second_evaluation_with_the_running_row` (aura): a held grant keeps the claim; the second evaluation answers the running row. |
| 5. non-empty executed under either name → 409 interrupted | whole 409 body | `executed_tombstones_refuse_with_the_interrupted_row` (aura); staging uses the resuming name only — the state a dead resume leaves. |
| 6. resuming + empty executed → rename-back → 409 parked with that document's blocking | whole 409 body + on-disk rename | `empty_resuming_document_renames_back_and_answers_the_parked_row` (aura): asserts the body and the resuming→parked file move. `concurrent_rename_back_losers_answer_the_parked_row_not_a_fault` (aura): two evaluations race the rename behind the test rendezvous gate; the loser's ENOENT proceeds against the restored parked name, no fault. |
| 7. fingerprint mismatch → 409 config_changed | whole 409 body + zero side effects on the checkpoint/ticket | `fingerprint_drift_refuses_with_the_config_changed_row` (aura): document and ticket must survive the refusal. Zero tool invocations and no consumed-decision cleanup are structural (fingerprint precedes consult) but their enforcement at the integration level is fill-time (exclusion below). |
| 8. ticket names another run/task/tool/args → 409 mismatch | whole 409 body | `approval_of_another_run_refuses_with_the_mismatch_row` (aura), the wrong-run sub-case; the task/tool/args sub-cases are unit-proven in `continuation.rs`'s tests and reuse the same wire row. |
| 9a. ticket missing INSIDE the window → 409 mismatch | whole 409 body | `missing_ticket_inside_the_window_refuses_with_the_mismatch_row` (aura). Document expiry 2099; the side is clock-choice independent. |
| 9b. ticket missing PAST expires_at → 409 expired (not mismatch) | whole 409 body | `missing_ticket_past_the_window_refuses_with_the_expired_row` (aura). Document expiry 2000; blocking carries the pre-sweep re-derivation. |
| 10. any pending undecided → 409 parked, blocking [{decision_id, tool, expires_at}] | whole 409 body | `undecided_calls_answer_the_parked_row_with_the_outstanding_set` (aura). |
| P58 ruled: approved executes once, sync-transparent | invocation pin + whole turns array | `approved_call_executes_once_and_rides_the_outcome_pair` (aura): exactly one RecordingTool invocation carrying the recorded arguments; the completed turns are the outcome-bearing pair — the assistant tool-call turn plus the tool-result turn holding the tool's real result, keyed by the original call id — ahead of the scripted final turn. |
| P58 ruled: denied steers with reason, sync-transparent | zero invocations + whole continuation context + whole turns array | `denied_call_steers_without_executing_and_rides_the_denial_pair` (aura): zero invocations; the continuation request's chat history pins the live denial text and its reason verbatim in place of the placeholder; the wire pair carries the live denial text keyed by the original call id; the segment completes with the scripted final turn (the worker adapts; no fabricated result). |
| P58 ruled: the placeholder never survives a decided resume | golden-greppable absence | both P58 frames assert the sentinel appears nowhere in the serialized turns, and the denial frame also in the continuation context (the fix contract's item 10). |
| R2: outcome-bearing turns on the 200 payload | the pair's wire shape, per decided call | `all_decided_grant_runs_the_segment_to_completion` + `re_park_mid_segment_carries_turns_and_the_new_blocking_entry` flipped to the pair-ahead shape (rows B below); `outcome_pair_and_sentinel_literals_match_the_wire_serializers` (aura) grounds the pair and sentinel literals against the implemented serializers, passing on arrival. |
| B. completed 200 | full 200 body, composite | `all_decided_grant_runs_the_segment_to_completion` (aura: segment turns as exact rig wire values — the R2 outcome-bearing pair ahead of the final turn, over the sentinel-carrying checkpoint) + `completed_segment_projects_the_full_200_body` (web: `from_segment` envelope, exact JSON). |
| B. parked-with-new-blocking 200 | segment half | `re_park_mid_segment_carries_turns_and_the_new_blocking_entry` (aura): turns pinned exactly — the R2 outcome-bearing pair ahead of the gated assistant turn, over the sentinel-carrying checkpoint — with the fresh decision id and expiry location-normalized after an audited shape check (a single entry carrying a UUID and an RFC 3339 stamp). |
| C. claim race | observable race frame | `concurrent_evaluations_admit_one_grant_and_refuse_the_loser_with_running` (aura): `tokio::join!`, exactly one grant, the loser's whole running-row body, either side winning. |
| D. blocking-population narrow reading | pinned by the bodies themselves | running/interrupted/config_changed/mismatch frames each pin `"blocking": []`; rename-back/parked/expired frames pin non-empty sets. The reading is enforced by literals, not prose. |
| —. stage order (first match wins) | ordering frame | `interrupted_outranks_expired` (aura): both conditions hold, the interrupted body wins. |
| —. wire serializer calibration | literal grounding | `blocking_entry_and_turn_literals_match_the_wire_serializers` (aura) and `outcome_pair_and_sentinel_literals_match_the_wire_serializers` (aura) — the tests that pass on arrival: they pin `BlockingEntry`'s and rig's implemented serializers against the literals the frames embed, including the R2 pair, the sentinel prompt, and the JSON-quoted wire forms of the sentinel and the live denial text. |

## Exclusions (not covered here, with owners)

| Excluded | Owner |
| --- | --- |
| The restart flow (kill/restart, `AURA_SESSION_STORE=file`) — process-death and re-boot integration over the claim table and documents. | Fill-time integration + Gate M (P38 rig extension). |
| The expired row's side effects (unlink checkpoint, sweep approvals, broker events under the request id). The 9b frame pins the body only; side effects are ruled into `evaluate_resume` but not asserted here. | Fill-time. |
| `config_changed` zero-invocation / no-cleanup enforcement at the tool layer (the frame pins the wire row plus the untouched document and ticket; it cannot observe tool dispatch). | Fill-time integration. |
| The memory-backend startup refusal (`refuse_park_on_memory_backend`) — a server-boot test. | Fill-time. |
| CLI anything. | P46 (out of scope). |
| The parked-200 envelope literal (`state: "parked"` + a blocking array on the 200 body). `BlockingEntry`/`ParkedToolName` are unconstructible outside the aura crate (constructors `pub(crate)`, no `Deserialize`), so `from_segment` cannot be fed a parked segment from the web-server suite. Blocked on a visibility ruling — see open question. | Board owner / fill unit. |

## Assumptions a green fill must honor (or consciously revise with the panel)

- The blocking entry's `expires_at` is the document's window stamp; the
  goldens pin document-derived expiries.
- The mismatch/expired row details pin today's `load_recorded_decisions`
  prose wrapped at the consult boundary; a wording change is a golden
  update, visible by design.
- The re-parked segment's turns are the R2 outcome-bearing pair per decided
  call, then the gated assistant turn (superseding the earlier
  parked-arm-only reading); a fill that drops or reorders the pair fails the
  frame and must be reconciled with the panel.
- The outcome pair's wire shape: the assistant tool-call turn carries the
  original call id in `ToolCall.id` with `call_id: null` (the checkpoint
  records no provider call id), and the tool-result turn carries the same id
  in `ToolResult.id` (the contract's "PendingCall.call_id matches
  ToolResult.id"); the tool-result text is the chain's JSON-quoted wire
  form. Grounded by the outcome-pair calibration test above.
- The denial literals mirror the gate's denial-feedback wording ("Tool call
  blocked by human approval denial: {reason}. Do not execute this action."),
  pinned by the gate's own unit tests; a wording change is a golden update,
  visible by design.
- The decided-resume fixtures stage the awaiting node's prompt as a live park
  leaves it: one sentinel tool result keyed by the pending call id, in its
  JSON-quoted wire form. A fill whose `replace_tool_result` misses that slot
  faults the resume — the pin working.
- `run_segment` consumes the `test_rig` worker-override queue for its
  continuation's worker builds, so the segment frames run scripted models.
- The handler resolves the resume config from `AppState.configs`; the
  handler frames supply exactly one config.
