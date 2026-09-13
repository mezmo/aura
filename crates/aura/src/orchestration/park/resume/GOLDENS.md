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
| Correction fold, fix-contract steps 6+8: the consumed set derives from the recorded set's before/after state, and a re-park removes only the actually-consumed subset after the commit publishes — untouched sibling nodes keep their recorded approvals | store-side pins across a two-resume lifecycle + the second resume's whole turns array | `consumed_subset_re_park_preserves_the_sibling_and_completes_on_the_second_resume` (aura): a two-node checkpoint (both decided) — resume 1 drives node A only (its decided call executes once, the continuation's new gated call re-parks, node B is not driven), then asserts node A's original decision is gone from the store, node B's decided ticket survives untouched (`try_parked` + the recorded approval), and exactly one fresh undecided ticket exists under the original bound run id (`list_pending` over the file store); resume 2 over the re-published checkpoint drives both nodes — the fresh and sibling calls each execute exactly once with their own arguments, the turns carry each decided call's R2 pair in segment order keyed by its own original call id, completion removes the fresh and sibling tickets together, and the placeholder appears nowhere. |
| Correction fold, fix-contract step 7: the strict guard drops before streaming, so a post-substitution new gated call (absent from the recorded set) re-parks through the LIVE arm and never faults as a strict miss; it re-arms on the next resume | whole both-resume chain — each segment's turns and blocking, no Err anywhere | `post_substitution_new_call_re_parks_through_the_live_arm_not_a_strict_miss` (aura): resume 1's parked segment carries the original pair keyed by the original call id ahead of the gated assistant turn (the A1 re-park pin; fresh decision id and expiry location-normalized) and resolves Ok — a strict-miss fault would fail the frame's own expect; resume 2 substitutes the fresh call and completes, its turns carrying the fresh pair keyed by the fresh call's id. |
| Correction fold, fix-contract step 4 second sentence, PARITY: an ordinary execution `Err` out of the substitution's `call_tool` becomes the tool-result text for the model (sync parity), never a fatal `SegmentError` | one invocation + whole turns array | `tool_failure_becomes_result_text_and_the_segment_completes` (aura): the staging vehicle is `FailingTool` — a gated tool under the decided call's name whose invocation always fails, through the same worker wrapper chain and tool-server path `RecordingTool` rides. The completed segment carries the R2 pair keyed by the original call id with the RAW error rendering the live loop delivers (`ToolServerError`'s `to_string`: `Toolset error: ToolCallError: ToolCallError: ToolCallError: …` — the loop's Err branch, not the JSON-quoted Ok form), ahead of the scripted final turn; no fabricated success, no placeholder. |
| Correction fold, fix-contract step 2 first half, FAULT: the decided entry missing at substitution time is a fatal `SegmentError` BEFORE any tombstone write or tool invocation | Err + the diagnostic + zero invocations + the on-disk executed list | `decided_entry_missing_at_substitution_time_is_fatal_before_the_tombstone` (aura): staging = the grant taken against the decided fixture, then the decision removed from BOTH surfaces a pre-flight may consult — the store ticket (`registry.remove`) and the grant's in-memory recorded entry (`RecordedDecisions::take` under the call's `CallKey`). Pins the strict-miss diagnostic: `resume mismatch: decided call kubectl_apply of task 3 is missing from the recorded set`. |
| Correction fold, fix-contract step 2 second half, FAULT: an APPROVED decision recorded without identity where the route demands identity is a fatal `SegmentError` before any tombstone or invocation (approvals only) | Err + the diagnostic + zero invocations + the on-disk executed list | `approved_without_required_identity_is_fatal_before_the_tombstone` (aura): staged over `identity_world` — the world variant whose poll-delivery webhook route (built the production way, `HitlRuntime::from_config`) carries a `tool_headers_from_response` mapping, arming `requires_identity` while keeping `park_registry` alive. Pins the gate's own wording verbatim: `resume mismatch: approved call is missing required approver identity` (gate.rs). |
| Correction fold, the identity-rule asymmetry: the same identity-demanding route never blocks a DENIAL — denials need no identity, so the denial's normal steer outcome holds and the identity fault never fires | whole turns array + zero invocations | `denied_without_identity_steers_normally_under_the_identity_route` (aura): over the same `identity_world`, a recorded denial (no identity, none required) completes with the denial pair — the live denial text keyed by the original call id — ahead of the scripted final turn; the frame's own `expect` fails if the identity fault ever fires. Stands alone from the approval fault so each half fails at its own named point. |
| Correction fold, fix-contract step 3, FAULT: a failing `append_executed_and_publish` is fatal before the invocation | Err + the diagnostic + zero invocations + the on-disk executed list unchanged | `failing_tombstone_write_is_fatal_before_the_invocation` (aura): staged by `stage_unwritable_tombstone_tmp` — a read-only leftover at the tombstone write's own temp path (`.run.resuming.json.tmp`), the filesystem-permission mechanism that survives the write's parent-directory tightening (`private_dir` re-opens directory permissions, so read-only directories alone cannot stage this). Pins the proven sequence's wording with the standard `EACCES` text: `resume tombstone write for call call_apply_1 failed: Permission denied (os error 13)`. |
| Correction fold, fix-contract step 5, FAULT: a checkpointed `current_prompt` with no tool-result slot for the call id makes `replace_tool_result` miss, which is fatal AFTER the tombstone and the invocation | Err + the diagnostic + exactly one invocation + the on-disk executed list carrying the call id | `replace_miss_is_fatal_after_the_tombstone_and_the_invocation` (aura): the fixture is the pre-A1 bare-prompt shape — `parked_document`'s `Message::user("tool results")`, exactly the no-slot prompt. Pins the proven sequence's wording verbatim: `continuation prompt has no tool result for call call_apply_1`. The frame's doc comment notes the designed consequence: the run is left in the interrupted state, and the next resume answers 409 `interrupted` on the once-only evidence. |
| Correction fold, Gate A round 1 (finding 1): same-key duplicate calls — identical tool and arguments, distinct call ids and decision ids — pair with their key's FIFO queue positionally; both decided approvals execute exactly once in order, both R2 pairs ride keyed by their OWN call ids, and the mid-segment re-park removes BOTH consumed ids (a front-only consumed derivation under-records the first call's decision and leaks its store row) | two invocations + both pairs on the re-parked turns + store-side pins across a two-resume lifecycle | `same_key_duplicate_calls_execute_once_each_and_a_re_park_removes_both_consumed_ids` (aura): one awaiting node holding two same-key pending calls, both tickets decided approved. Resume 1's re-parked segment carries both outcome pairs keyed by `call_apply_1` and `call_apply_2` ahead of the gated assistant turn; the store then holds NEITHER duplicate decision (both removed by the consumed-subset cleanup, each derived from its own key's queue-depth drop) and exactly one fresh undecided ticket under the original bound run id. Resume 2 drives the fresh call and completes; the duplicate calls are not re-executed, and completion clears all three rows. |
| Correction fold, Gate A round 1 (finding 1), FAULT: a recorded queue one entry short of the pending sequence faults at the missing position — the second same-key call — before any tombstone or invocation | Err + the diagnostic + zero invocations + the on-disk executed list | `second_same_key_entry_missing_is_fatal_before_the_tombstone` (aura): staging = the grant over the both-decided duplicate fixture, then the queue is left one entry short for two calls — the second ticket leaves the store (`registry.remove`) and one entry leaves the recorded queue (`take` under the shared key). Pins the strict-miss wording naming the faulting call's tool and task: `resume mismatch: decided call kubectl_apply of task 3 is missing from the recorded set` — byte-identical to the single-call frame's literal, the duplicates sharing tool and task. |
| Correction fold, Gate A round 1 (finding 1), FAULT: an approval recorded without identity at the queue's second position faults at that position under the identity route (approvals only), before any tombstone or invocation — a front-only pre-flight would have passed it | Err + the diagnostic + zero invocations + the on-disk executed list | `second_same_key_entry_identity_blocked_is_fatal_before_the_tombstone` (aura): over `identity_world`, the first duplicate records with its captured identity, the second without. Pins the gate's identity wording verbatim at the second position: `resume mismatch: approved call is missing required approver identity`. |
| Correction fold, Gate A round 1 (finding 2), FAULT: an awaiting node with an absent (or empty) pending list faults the segment in the driver's seeding loop, before any worker build — the consult skips such nodes, so the run still grants, and streaming the checkpointed prompt would carry the stale placeholder past the substitution | Err + the diagnostic + an unconsumed worker override + an empty scripted-model request log + the on-disk executed list | `awaiting_node_without_pending_calls_faults_the_segment_before_any_worker_builds` (aura): a mixed checkpoint — one malformed awaiting node (task 3, pending: None, riding ahead of one genuinely decided node). Pins `awaiting task 3 carries no pending calls in the checkpoint`; the queued worker override is still queued after the fault (no build consumed it, so no worker streamed) and the scripted model's request log is empty. Legitimate checkpoints never trip it: the gate registers every parked call durably before the park commits, and the re-park refresh retains decided and undecided calls alike. `awaiting_node_with_an_empty_pending_list_faults_before_any_worker_builds` (aura): the pending-EMPTY half — same mixed checkpoint with `pending: Some(vec![])`, pinning `awaiting task 3 carries an empty pending list in the checkpoint` and the same no-build/no-stream/no-tombstone pins. |
| —. Reconstruction direction (R5, ruled 2026-09-12), Stage 1 fixture correction: the two-slot duplicate fixture is faithful to the genuine same-completion shape | fixture shape only; the consuming frames' pins unchanged | `duplicate_key_document` (aura, fixture): the node's history now carries the assistant turn that issued BOTH gated calls (one message, one tool call per call — the producer's reachable same-completion park, the Gate M vet's finding 5) ahead of the two-slot sentinel prompt. The three consuming frames (`same_key_duplicate_calls_execute_once_each_and_a_re_park_removes_both_consumed_ids`, `second_same_key_entry_missing_is_fatal_before_the_tombstone`, `second_same_key_entry_identity_blocked_is_fatal_before_the_tombstone`) stay green byte-identical. |
| Reconstruction (R5, Stage 1), PRE-FAILING: the pivot shape — two pending calls on DIFFERENT tools under one node, ONE sentinel slot (the gate-tripping first call's), the second call's tool call ABSENT from the captured history — both approved: each call executes exactly once in document order, the segment completes, and no sentinel survives into the continuation | two invocations in document order + completion + the outcome pairing pinned structurally (exactly the two tool results in the reconstructed context, keyed to their own call ids in document order, each preceded by its matching assistant tool call — the second call's call present only by the builder's synthesis, naming the pending call's tool and arguments) + sentinel-absence in the turns and the reconstructed context + store cleanup | `pivot_approved_pair_executes_once_each_in_document_order_and_completes` (aura), over `pivot_two_call_document`. PRE-FAILING SPEC FRAME (Stage 1): fails today on the fold's replace-miss fatal — `continuation prompt has no tool result for call call_scale_2` — landing after BOTH invocations and BOTH tombstones; flips green when Stage 3 lands the context builder. |
| Reconstruction (R5, Stage 1), PRE-FAILING: approve/deny over the pivot shape — the approved first call executes exactly once, the denied second call never executes, and the live denial text with its reason is what the model sees for the second call; the segment completes | one invocation + zero on the denied tool + the outcome pairing pinned structurally (the denial rides in the tool result keyed to the SECOND call's own id — not merely somewhere in the context — preceded by its synthesized assistant call; the approved call's real result keyed to the first call's id) + completion | `pivot_approve_then_deny_executes_only_the_approved_call_and_steers_the_second` (aura). PRE-FAILING SPEC FRAME (Stage 1): fails today on the same replace-miss fatal — `continuation prompt has no tool result for call call_scale_2` — landing after the approved call's invocation and both tombstones. |
| Reconstruction (R5, Stage 1), PRE-FAILING: deny/deny over the pivot shape — the Gate M deny leg — ZERO invocations, both denial texts delivered to the model, and the segment completes | zero invocations + both denials pinned structurally (each tool result keyed to its OWN call id — the two denials share their reason, so only id-keyed pairing catches duplicating the first denial onto the second's slot or omitting the second's outcome — each preceded by its matching assistant tool call) + completion | `pivot_denied_pair_steers_without_executing_and_completes` (aura). PRE-FAILING SPEC FRAME (Stage 1): fails today on the same replace-miss fatal — `continuation prompt has no tool result for call call_scale_2` — landing after both tombstones and zero invocations: the live deny leg's exact once-only interrupted state. |
| —. run-id binding on the re-parked fresh ticket | the fresh ticket's owner id and worker scope | `re_park_registers_the_fresh_ticket_under_the_original_bound_run_id` (aura) — the seam unit's one authorized golden addition, whose manifest row this file owed (the gap ruled into the A2 unit). Its fixture carries the sentinel document and a RecordingTool for the decided tool (the board-owner repair ruling, logged on the card): without them the B1 fill's substitution would fault for fixture reasons — a missing ToolResult slot to replace, a missing tool to invoke. The frame's pin is the run-id binding only; execution-count assertions live in the P58 and lifecycle frames. |
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
- The duplicate fixture stages the genuine same-completion two-call park
  (the producer CAN write two sentinel slots when one completion issues both
  gated calls): the node's history carries the assistant turn with both tool
  calls, and the prompt carries both slots. The pivot fixture
  (`pivot_two_call_document`) mirrors the Gate M deny-leg checkpoint
  (evidence 2026-09-12-p45-fold-gate-m): a live park writes ONE sentinel —
  for the gate-tripping call only; a pending call whose tool call the
  snapshot missed (the re-driven worker's pivot) has NO slot and no history
  entry. The three pivot frames pin the ruled reconstruction (R5) over that
  shape, with the outcome pairing asserted structurally from the captured
  continuation request: the model's context carries exactly the pending
  calls' tool results, keyed to their own call ids in document order, each
  preceded by its matching assistant tool call — the pairing the Stage 2
  builder must normalize (no orphaned results, no missing calls). The
  frames stay red on the fold's replace-miss fatal until the Stage 3
  context builder lands, and `replace_tool_result` reaps in Stage 7.
- `run_segment` consumes the `test_rig` worker-override queue for its
  continuation's worker builds, so the segment frames run scripted models.
- The two-resume lifecycle frames install their worker overrides per resume,
  never up front: the queue is take-once and process-global, and a frame
  failing mid-lifecycle must not leak the resumes it never drove into the
  next consumer's builds. Resume 1 consumes one override (the segment
  returns at the first re-park, so later awaiting nodes are not driven);
  resume 2 consumes one per driven node, in plan order (node A's build, then
  node B's).
- The re-parked fresh call's pending id is the rig tool-call id the park's
  `take_current_call_id` stashed (`call_0` in the scripts) — not the
  scripted provider call id, which rides only the gated assistant turn's
  wire (`call_id_0`). The fresh call's R2 pair keys by that pending id with
  `call_id: null`, the same reading as the original-park pair.
- Resume 1's parked-200 blocking derives from the commit's refreshed
  `pending_by_task`, which retains decided siblings for the resume consult,
  so node B's decided entry can ride the list alongside the fresh one. The
  lifecycle frame selects the fresh entry by tool name (exactly one entry
  names the newly gated tool) and pins the outstanding set on the store
  (`list_pending`), not on the wire list; whether the parked-200 blocking
  should be outstanding-only — the 409 rows' narrow reading — is a B1 /
  U(endpoint) reconciliation point, deliberately not pinned here.
- The handler resolves the resume config from `AppState.configs`; the
  handler frames supply exactly one config.
- The A3 fault frames pin the exact `SegmentError::Continuation`
  diagnostics a green fill must produce, because the variant alone cannot
  discriminate the four fault rows: the strict miss
  (`resume mismatch: decided call kubectl_apply of task 3 is missing from
  the recorded set`), the identity block (the gate's own wording,
  `resume mismatch: approved call is missing required approver identity`),
  the tombstone failure (the proven sequence's wording over the standard
  `EACCES` text, `resume tombstone write for call call_apply_1 failed:
  Permission denied (os error 13)`), and the replace miss (the proven
  sequence's wording, `continuation prompt has no tool result for call
  call_apply_1`). A fill wording these differently flips the frames and
  must be reconciled with the panel, not hand-accepted.
- The parity frame pins the error rendering the live multi-turn loop
  delivers for a tool-server failure — the `ToolServerError`'s raw
  `to_string` (`Toolset error: ToolCallError: ToolCallError:
  ToolCallError: …`), grounded in the rig fork's loop (the Err branch
  renders `e.to_string()` raw; only the Ok path JSON-encodes). The
  denial short-circuit rides the Ok path and stays JSON-quoted; an
  execution `Err` rides the Err path and stays raw. B1's substitution maps
  its `call_tool` errors this same way.
- The same-key duplicate frames pin positional pairing: the pending
  calls in document order pair with their key's FIFO queue
  front-to-back (the i-th same-key call pre-flights against queue
  position i), and the consumed set derives from the key's queue depth
  around each call's own invocation — a drop of exactly one is that
  call's decision consumed. A fill that pairs by front-only peek or
  derives consumption from a passed pre-flight fails the lifecycle
  frame's store-side pins and the second-position fault frames.
- The identity frames' world variant (`identity_world`) builds its route
  the production way (`HitlRuntime::from_config`) with poll delivery and a
  `tool_headers_from_response` mapping: poll keeps `park_registry` (the
  segment driver's park seam) alive while the mapping arms
  `requires_identity`. The webhook URL is unreachable and never
  consulted — a recorded hit short-circuits at the gate consult.
- The tombstone-failure staging keys on the tombstone write's own temp
  path (`.{run}.resuming.json.tmp`, the name `append_executed_and_publish`
  derives). Directory-permission staging cannot work: the write tightens
  its parent to owner-writable (`private_dir`) before opening the temp
  file, so only the read-only leftover at the temp path itself faults the
  write.
