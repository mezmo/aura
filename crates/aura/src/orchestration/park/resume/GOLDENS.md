# P45 golden coverage manifest

Map of the ruled resume rows (2026-09-12, card P45) to their whole-frame
golden fixtures, with the exclusions the goldens do not cover. The frames
live in two suites, split along the visibility wall the DESIGN.md seam table
records:

- **Pipeline frames** —
  `orchestration::park::resume::goldens` (this directory, `goldens.rs`):
  drive `evaluate_resume` / `run_segment_borrowed` (the borrowed-grant
  stream seam; the atomic `run_segment` entry was retired at S6 and the
  migrated frames drive a `run_segment_live` test helper over the same
  seam) over production-reachable
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
| P58 ruled: approved executes once, sync-transparent | invocation pin + whole continuation context + whole turns array | `approved_call_executes_once_and_rides_the_outcome_pair` (aura): exactly one RecordingTool invocation carrying the recorded arguments; the outcome package rides the REBUILT continuation context (see the denied frame below) and the completed turns are the natural continuation turn plus the scripted coordinator tail (R6/F2: the wire pair itself rides the re-parked arm only). |
| P58 ruled: denied steers with reason, sync-transparent | zero invocations + whole continuation context + whole turns array | `denied_call_steers_without_executing_and_rides_the_denial_pair` (aura): zero invocations; the continuation request's chat history pins the decided call's assistant tool call — synthesized by the reconstruction (the P45 stage-3 wiring; the single-call sentinel fixture's history never captured it), the pairing providers require — ahead of the live denial text and its reason verbatim in place of the placeholder; the segment completes with the scripted final turn and the scripted coordinator tail (the worker adapts; no fabricated result). The denial's wire pair rides the re-parked arm only (R6/F2). |
| P58 ruled: the placeholder never survives a decided resume | golden-greppable absence | both P58 frames assert the sentinel appears nowhere in the serialized turns, and the denial frame also in the continuation context (the fix contract's item 10). |
| R2: outcome-bearing turns on the 200 payload | the pair's wire shape, per decided call | `re_park_mid_segment_carries_turns_and_the_new_blocking_entry` pins the pair-ahead-of-the-gated-turn shape on the RE-PARKED arm (rows B below); since F2 (the R6 natural-finish fill) the COMPLETED turns carry the run's natural turns only — the pairs come off the completed wire because the outcome packages live inside the rebuilt histories the continuations streamed from; `outcome_pair_and_sentinel_literals_match_the_wire_serializers` (aura) still grounds the pair and sentinel literals against the implemented serializers (the re-parked arm renders them). |
| B. completed 200 | full 200 body, composite | `all_decided_grant_runs_the_segment_to_completion` (aura, updated at F2: the old pin encoded the pre-R6 truncation — the pair-on-completed-turns and the missing coordinator tail; the pin is now the natural continuation turn plus the scripted coordinator tail over the sentinel-carrying checkpoint) + `completed_segment_projects_the_full_200_body` (web: `from_segment` envelope, exact JSON — the pre-R6 segment shape; the R6 wire frames below supersede its completed-arm reading). |
| B. parked-with-new-blocking 200 | segment half | `re_park_mid_segment_carries_turns_and_the_new_blocking_entry` (aura): turns pinned exactly — the R2 outcome-bearing pair ahead of the gated assistant turn, over the sentinel-carrying checkpoint — with the fresh decision id and expiry location-normalized after an audited shape check (a single entry carrying a UUID and an RFC 3339 stamp). |
| Correction fold, fix-contract steps 6+8: the consumed set derives from the recorded set's before/after state, and a re-park removes only the actually-consumed subset after the commit publishes — untouched sibling nodes keep their recorded approvals | store-side pins across a two-resume lifecycle + the second resume's whole turns array | `consumed_subset_re_park_preserves_the_sibling_and_completes_on_the_second_resume` (aura): a two-node checkpoint (both decided) — resume 1 drives node A only (its decided call executes once, the continuation's new gated call re-parks, node B is not driven), then asserts node A's original decision is gone from the store, node B's decided ticket survives untouched (`try_parked` + the recorded approval), and exactly one fresh undecided ticket exists under the original bound run id (`list_pending` over the file store); resume 2 over the re-published checkpoint drives both nodes — the fresh and sibling calls each execute exactly once with their own arguments — and completes through the coordinator continuation (F2/R6): the completed turns are the two nodes' natural final turns plus the scripted coordinator tail, the R2 pairs having ridden the re-parked first segment in segment order keyed by their own original call ids; completion removes the fresh and sibling tickets together, and the placeholder appears nowhere. |
| Correction fold, fix-contract step 7: the strict guard drops before streaming, so a post-substitution new gated call (absent from the recorded set) re-parks through the LIVE arm and never faults as a strict miss; it re-arms on the next resume | whole both-resume chain — each segment's turns and blocking, no Err anywhere | `post_substitution_new_call_re_parks_through_the_live_arm_not_a_strict_miss` (aura): resume 1's parked segment carries the original pair keyed by the original call id ahead of the gated assistant turn (the A1 re-park pin; fresh decision id and expiry location-normalized) and resolves Ok — a strict-miss fault would fail the frame's own expect; resume 2 substitutes the fresh call and completes through the coordinator continuation (F2/R6): the natural final turn plus the scripted coordinator tail (the fresh pair rode the re-parked first segment under its own id). |
| Correction fold, fix-contract step 4 second sentence, PARITY: an ordinary execution `Err` out of the substitution's `call_tool` becomes the tool-result text for the model (sync parity), never a fatal `SegmentError` | one invocation + whole REBUILT continuation context + whole turns array | `tool_failure_becomes_result_text_and_the_segment_completes` (aura): the staging vehicle is `FailingTool` — a gated tool under the decided call's name whose invocation always fails, through the same worker wrapper chain and tool-server path `RecordingTool` rides. Since F2/R6 the error rendering's ground truth is the REBUILT continuation context, and the F2 follow-up (board-owner ruling) re-pins it THERE by exact equality: the continuation request's chat history carries the decided call's assistant tool call ahead of the RAW `ToolServerError` `to_string` the substitution maps (`Toolset error: ToolCallError: ToolCallError: ToolCallError: …` — the retired `tool_failure_wire` literal's guarantee restored at its new home: execution Err becomes result text, never fabricated success, byte-stable formats). The completed segment carries the natural final turn plus the scripted coordinator tail; no placeholder. |
| Correction fold, fix-contract step 2 first half, FAULT: the decided entry missing at substitution time is a fatal `SegmentError` BEFORE any tombstone write or tool invocation | Err + the diagnostic + zero invocations + the on-disk executed list | `decided_entry_missing_at_substitution_time_is_fatal_before_the_tombstone` (aura): staging = the grant taken against the decided fixture, then the decision removed from BOTH surfaces a pre-flight may consult — the store ticket (`registry.remove`) and the grant's in-memory recorded entry (`RecordedDecisions::take` under the call's `CallKey`). Pins the strict-miss diagnostic: `resume mismatch: decided call kubectl_apply of task 3 is missing from the recorded set`. |
| Correction fold, fix-contract step 2 second half, FAULT: an APPROVED decision recorded without identity where the route demands identity is a fatal `SegmentError` before any tombstone or invocation (approvals only) | Err + the diagnostic + zero invocations + the on-disk executed list | `approved_without_required_identity_is_fatal_before_the_tombstone` (aura): staged over `identity_world` — the world variant whose poll-delivery webhook route (built the production way, `HitlRuntime::from_config`) carries a `tool_headers_from_response` mapping, arming `requires_identity` while keeping `park_registry` alive. Pins the gate's own wording verbatim: `resume mismatch: approved call is missing required approver identity` (gate.rs). |
| Correction fold, the identity-rule asymmetry: the same identity-demanding route never blocks a DENIAL — denials need no identity, so the denial's normal steer outcome holds and the identity fault never fires | whole turns array + zero invocations | `denied_without_identity_steers_normally_under_the_identity_route` (aura): over the same `identity_world`, a recorded denial (no identity, none required) completes with its natural steer outcome — the continuation's final turn plus the scripted coordinator tail (F2/R6; the denial package rides the rebuilt history); the frame's own `expect` fails if the identity fault ever fires. Stands alone from the approval fault so each half fails at its own named point. |
| Correction fold, fix-contract step 3, FAULT: a failing `append_executed_and_publish` is fatal before the invocation | Err + the diagnostic + zero invocations + the on-disk executed list unchanged | `failing_tombstone_write_is_fatal_before_the_invocation` (aura): staged by `stage_unwritable_tombstone_tmp` — a read-only leftover at the tombstone write's own temp path (`.run.resuming.json.tmp`), the filesystem-permission mechanism that survives the write's parent-directory tightening (`private_dir` re-opens directory permissions, so read-only directories alone cannot stage this). Pins the proven sequence's wording with the standard `EACCES` text: `resume tombstone write for call call_apply_1 failed: Permission denied (os error 13)`. |
| Reconstruction wiring (P45 stage 3), FAULT: a checkpointed `current_prompt` with NO tool result at all — the pre-A1 bare-prompt shape, a checkpoint no park producer writes — is refused by the segment-wide preflight, fatally, BEFORE any tombstone or invocation across the whole segment | Err + the node-attributed diagnostic + zero invocations + an empty on-disk executed list + an unconsumed worker override and empty model-request log | `tool_result_less_prompt_refuses_at_preflight_before_any_tombstone` (aura): the same bare-prompt fixture the fold's replace-miss frame staged — `parked_document`'s `Message::user("tool results")`. Pins the node-attributed `NotAToolResultPrompt` wording: `awaiting node 0: the parked snapshot's current prompt is not the tool-result message the park producers write`. Record: this row RETIRES the fold's after-tombstone replace-miss fatal (fix-contract step 5's `replace_miss_is_fatal_after_the_tombstone_and_the_invocation`, pinning `continuation prompt has no tool result for call call_apply_1` after one invocation and one tombstone) — the reconstruction wiring removed the replace whose miss it pinned, so the refusal now fires at the preflight, before the first tombstone. The interrupted-state semantics for post-tombstone crashes stay pinned by `executed_tombstones_refuse_with_the_interrupted_row`. |
| Reconstruction wiring (P45 stage 3), the segment door, integration: a two-awaiting-node checkpoint where node A is fully valid and node B's pending call carries an EMPTY call id — the shape the gate's park arm records when the stream hook observed no tool-call id — is refused with the node-attributed `EmptyCallId` diagnostic naming node B, and NOTHING runs: node A's valid input must not execute first | Err + the diagnostic + zero invocations across the segment + zero tombstones + an unconsumed worker override and empty model-request log | `empty_call_id_on_node_b_refuses_the_whole_segment_before_any_tombstone` (aura): both nodes' decisions recorded over the two-node fixture with node B's pending call id emptied. Pins `awaiting node 1: pending call member 0 carries an empty call id` — the all-or-nothing property the rebuild unit frames pin at the door, here at the wired drive. |
| Correction fold, Gate A round 1 (finding 1): same-key duplicate calls — identical tool and arguments, distinct call ids and decision ids — pair with their key's FIFO queue positionally; both decided approvals execute exactly once in order, both R2 pairs ride keyed by their OWN call ids, and the mid-segment re-park removes BOTH consumed ids (a front-only consumed derivation under-records the first call's decision and leaks its store row) | two invocations + both pairs on the re-parked turns + store-side pins across a two-resume lifecycle | `same_key_duplicate_calls_execute_once_each_and_a_re_park_removes_both_consumed_ids` (aura): one awaiting node holding two same-key pending calls, both tickets decided approved. Resume 1's re-parked segment carries both outcome pairs keyed by `call_apply_1` and `call_apply_2` ahead of the gated assistant turn; the store then holds NEITHER duplicate decision (both removed by the consumed-subset cleanup, each derived from its own key's queue-depth drop) and exactly one fresh undecided ticket under the original bound run id. Resume 2 drives the fresh call and completes; the duplicate calls are not re-executed, and completion clears all three rows. |
| Correction fold, Gate A round 1 (finding 1), FAULT: a recorded queue one entry short of the pending sequence faults at the missing position — the second same-key call — before any tombstone or invocation | Err + the diagnostic + zero invocations + the on-disk executed list | `second_same_key_entry_missing_is_fatal_before_the_tombstone` (aura): staging = the grant over the both-decided duplicate fixture, then the queue is left one entry short for two calls — the second ticket leaves the store (`registry.remove`) and one entry leaves the recorded queue (`take` under the shared key). Pins the strict-miss wording naming the faulting call's tool and task: `resume mismatch: decided call kubectl_apply of task 3 is missing from the recorded set` — byte-identical to the single-call frame's literal, the duplicates sharing tool and task. |
| Correction fold, Gate A round 1 (finding 1), FAULT: an approval recorded without identity at the queue's second position faults at that position under the identity route (approvals only), before any tombstone or invocation — a front-only pre-flight would have passed it | Err + the diagnostic + zero invocations + the on-disk executed list | `second_same_key_entry_identity_blocked_is_fatal_before_the_tombstone` (aura): over `identity_world`, the first duplicate records with its captured identity, the second without. Pins the gate's identity wording verbatim at the second position: `resume mismatch: approved call is missing required approver identity`. |
| Correction fold, Gate A round 1 (finding 2), FAULT: an awaiting node with an absent (or empty) pending list faults the segment in the driver's seeding loop, before any worker build — the consult skips such nodes, so the run still grants, and streaming the checkpointed prompt would carry the stale placeholder past the substitution | Err + the diagnostic + an unconsumed worker override + an empty scripted-model request log + the on-disk executed list | `awaiting_node_without_pending_calls_faults_the_segment_before_any_worker_builds` (aura): a mixed checkpoint — one malformed awaiting node (task 3, pending: None, riding ahead of one genuinely decided node). Pins `awaiting task 3 carries no pending calls in the checkpoint`; the queued worker override is still queued after the fault (no build consumed it, so no worker streamed) and the scripted model's request log is empty. Legitimate checkpoints never trip it: the gate registers every parked call durably before the park commits, and the re-park refresh retains decided and undecided calls alike. `awaiting_node_with_an_empty_pending_list_faults_before_any_worker_builds` (aura): the pending-EMPTY half — same mixed checkpoint with `pending: Some(vec![])`, pinning `awaiting task 3 carries an empty pending list in the checkpoint` and the same no-build/no-stream/no-tombstone pins. |
| —. Reconstruction direction (R5, ruled 2026-09-12), Stage 1 fixture correction: the two-slot duplicate fixture is faithful to the genuine same-completion shape | fixture shape only; the consuming frames' pins unchanged | `duplicate_key_document` (aura, fixture): the node's history now carries the assistant turn that issued BOTH gated calls (one message, one tool call per call — the producer's reachable same-completion park, the Gate M vet's finding 5) ahead of the two-slot sentinel prompt. The three consuming frames (`same_key_duplicate_calls_execute_once_each_and_a_re_park_removes_both_consumed_ids`, `second_same_key_entry_missing_is_fatal_before_the_tombstone`, `second_same_key_entry_identity_blocked_is_fatal_before_the_tombstone`) stay green byte-identical. |
| Reconstruction (R5, Stage 1), flipped green by the stage-3 wiring: the pivot shape — two pending calls on DIFFERENT tools under one node, ONE sentinel slot (the gate-tripping first call's), the second call's tool call ABSENT from the captured history — both approved: each call executes exactly once in document order, the segment completes, and no sentinel survives into the continuation | two invocations in document order + completion + the outcome pairing pinned structurally (exactly the two tool results in the reconstructed context, keyed to their own call ids in document order, each preceded by its matching assistant tool call — the second call's call present only by the builder's synthesis, naming the pending call's tool and arguments) + sentinel-absence in the turns and the reconstructed context + store cleanup | `pivot_approved_pair_executes_once_each_in_document_order_and_completes` (aura), over `pivot_two_call_document`. FLIPPED GREEN by the P45 stage-3 reconstruction wiring (this change's commit), unedited: the frame was red on the fold's replace-miss fatal — `continuation prompt has no tool result for call call_scale_2` — until the segment preflight, keyed resolution, and `rebuild_context` landed. |
| Reconstruction (R5, Stage 1), flipped green by the stage-3 wiring: approve/deny over the pivot shape — the approved first call executes exactly once, the denied second call never executes, and the live denial text with its reason is what the model sees for the second call; the segment completes | one invocation + zero on the denied tool + the outcome pairing pinned structurally (the denial rides in the tool result keyed to the SECOND call's own id — not merely somewhere in the context — preceded by its synthesized assistant call; the approved call's real result keyed to the first call's id) + completion | `pivot_approve_then_deny_executes_only_the_approved_call_and_steers_the_second` (aura). FLIPPED GREEN by the P45 stage-3 reconstruction wiring (this change's commit), unedited: red on the same replace-miss fatal until the wiring landed. |
| Reconstruction (R5, Stage 1), flipped green by the stage-3 wiring: deny/deny over the pivot shape — the Gate M deny leg — ZERO invocations, both denial texts delivered to the model, and the segment completes | zero invocations + both denials pinned structurally (each tool result keyed to its OWN call id — the two denials share their reason, so only id-keyed pairing catches duplicating the first denial onto the second's slot or omitting the second's outcome — each preceded by its matching assistant tool call) + completion | `pivot_denied_pair_steers_without_executing_and_completes` (aura). FLIPPED GREEN by the P45 stage-3 reconstruction wiring (this change's commit), unedited: red on the same replace-miss fatal until the wiring landed. |
| Reconstruction wiring (P45 stage 3), the re-park turn boundary: the turns a re-parking segment reports for the re-parked node start strictly AFTER the rebuilt history the continuation actually streamed from — the reconstructed input, the synthesized second call's turn included, is never re-emitted as segment turns | whole Parked turns array + the re-park commit's refreshed blocking | `re_parked_turns_start_after_the_rebuilt_history_with_no_replayed_reconstruction` (aura): the pivot fixture, both calls approved, the continuation issuing a fresh gated call mid-segment (modeled on `re_park_mid_segment_carries_turns_and_the_new_blocking_entry`). Pins the turns EXACTLY — the two decided pairs keyed by their own call ids, then the gated assistant turn, nothing else — with the fresh decision id and expiry location-normalized after the audited shape check. A boundary sliced at the CHECKPOINT's recorded history length would replay the synthesized turn here; the wiring threads the rebuilt history's length through instead. |
| —. run-id binding on the re-parked fresh ticket | the fresh ticket's owner id and worker scope | `re_park_registers_the_fresh_ticket_under_the_original_bound_run_id` (aura) — the seam unit's one authorized golden addition, whose manifest row this file owed (the gap ruled into the A2 unit). Its fixture carries the sentinel document and a RecordingTool for the decided tool (the board-owner repair ruling, logged on the card): without them the B1 fill's substitution would fault for fixture reasons — a missing ToolResult slot to replace, a missing tool to invoke. The frame's pin is the run-id binding only; execution-count assertions live in the P58 and lifecycle frames. |
| C. claim race | observable race frame | `concurrent_evaluations_admit_one_grant_and_refuse_the_loser_with_running` (aura): `tokio::join!`, exactly one grant, the loser's whole running-row body, either side winning. |
| D. blocking-population narrow reading | pinned by the bodies themselves | running/interrupted/config_changed/mismatch frames each pin `"blocking": []`; rename-back/parked/expired frames pin non-empty sets. The reading is enforced by literals, not prose. |
| —. stage order (first match wins) | ordering frame | `interrupted_outranks_expired` (aura): both conditions hold, the interrupted body wins. |
| —. wire serializer calibration | literal grounding | `blocking_entry_and_turn_literals_match_the_wire_serializers` (aura) and `outcome_pair_and_sentinel_literals_match_the_wire_serializers` (aura) — the tests that pass on arrival: they pin `BlockingEntry`'s and rig's implemented serializers against the literals the frames embed, including the R2 pair, the sentinel prompt, and the JSON-quoted wire forms of the sentinel and the live denial text. |
| Stage 6 (R6 natural-finish ruling): the resume must resume the COORDINATOR ITERATION LOOP — restore the coordinator conversation, routing, iteration, and failure history from the checkpoint and continue through plan_with_routing — not just re-enter the executor: after the awaiting node's approved call executes exactly once, a never-started Pending sibling RUNS, the run completes with the coordinator's natural final-answer turns beyond the last worker turn, and completion still deletes the checkpoint and removes the consumed decisions | sibling probe invocation + approved-call invocation count + final-answer PRESENCE beyond the sibling's last turn + on-disk checkpoint deletion + store cleanup | `coordinator_resumes_after_awaiting_nodes_and_drives_never_started_siblings_to_completion` (aura): FLIPPED GREEN at the F2 follow-up under the board-owner fixture-repair ruling — the `SIBLING_DONE` marker moved onto the submit_result turn's own text (`.with_text`, streaming ahead of the tool call in the same turn) and the never-requested trailing `ScriptedTurn::text` dropped from the sibling's script: the old script assumed a trailing text turn the live `submit_result` decision short-circuit never requests (`drive_forward_loop` ends the stream at the submit_result tool result, one item past it) — an unfaithful producer shape, not a coordinator defect. The marker-scan pin is byte-identical: the sibling runs (probe == 1), the coordinator tail lands beyond the marker's turn, completion deletes the checkpoint, and the store clears. |
| Stage 6 (R6), Gate A round 1 (finding 1): a resumed worker's failure must reach the coordinator loop's DECISION CONTEXT — the resumed node maps through the live loop's soft-failure rule, so the continuation request carries it as failed plan state and the failure history records it under the resumed iteration; the loop then continues past it (a replacement or retried task's worker runs OR a final answer lands) and the run completes; the approved call still executed exactly once | approved-call count + decision-context pins (the continuation request's prompt carries the FAILED TASKS line and the Iteration-2 failure-history line, by exact literal) + the loop-continuation disjunct (replacement probe invocation == 1 OR an assistant text turn after the failure report) + completion + cleanup | `resumed_coordinator_replans_when_a_resumed_worker_fails` (aura): the coordinator's scripted model is cloned before install so the frame reads the request log the consumed override recorded — the strengthened re-plan pin (the pre-fix fabricated-completion shape fails it); the failure report rides the turns, the scripted respond_directly lands the disjunct's final-answer leg, completion deletes the checkpoint and clears the store. |
| Stage 6, Gate A round 1 (finding 2): a checkpoint's restored failures ride into the resumed loop's failure history EXACTLY ONCE — under the parked iteration — never re-recorded under the resumed iteration; the restored node stays visible as failed plan state and the awaiting node completes | the continuation request's prompt: the restored Iteration-1 history line exactly once, NO Iteration-2 line for the restored description, no OBSERVED PATTERNS section, the restored node's FAILED TASKS line, and the completed awaiting node's confidence line | `restored_failures_are_not_re_recorded_under_the_resumed_iteration` (aura): one pre-existing Failed node (agent_timeout, its record seeded in the checkpoint's failure history under the parked iteration) plus one awaiting node over the standard sentinel fixture; the coordinator stays scripted (respond_directly) and the frame reads the cloned coordinator model's request log (the pre-fix duplication renders both history lines plus a false repeated-failure pattern). |
| Stage 6: `segment_plan` must restore the CHECKPOINT's `plan.goal`, not rebuild the plan from the raw query | the re-published checkpoint's `plan.goal` (and `query`, unchanged) | `segment_plan_restores_the_checkpoint_goal_not_the_query` (aura): query and goal are deliberately different strings; the continuation re-parks on a fresh gated call and the frame loads the re-published parked document — the run-level projection of the segment plan's goal through the re-park commit (`build_document` stamps `plan.goal`), so a goal=query reconstruction writes the query back into the checkpoint. The prompt-level observables (a re-plan prompt or final answer echoing the goal) belong to the later wire-level R6 unit. |
| Stage 6, completion-path grant-cleanup audit (verify-then-maybe-pin) | verification only — nothing to pin | Audited for this unit: the only completion-path cleanup site is the `grant.consumed_decisions()` loop after the resuming-document unlink. `ResumeGrant.consumed` is the consult's decided-consumed list (`load_recorded_decisions`), the consult refuses while any pending call is undecided, and a Completed segment drove every awaiting node to Normal — so the completion-path removal set coincides with the actually-consumed set; the re-park path removes the per-invocation accumulator instead. NO completion-path site removes all loaded ids — the vet finding was pre-fixed by the F1 positional repair. The remaining `registry.remove` sites are the conversational route's timeout arm, the Orphaned-fault cancel, the expired-refusal sweep, and tests. |
| Phase A frontier review round 1 (finding 2), pair retention, EARLY re-park: a parked segment's turns carry every decided pair consumed in the segment — a COMPLETED node's pairs ride when a sibling re-parks, each node's pairs ahead of that node's own turns (document order for a completed node, ahead of the gated turn for a re-parked one) | whole Parked turns array + blocking (location-normalized fresh entry) + per-tool invocation counts | `a_completed_nodes_pairs_ride_the_early_re_park` (aura): the two-node fixture, node A completing through submit_result and node B's continuation re-parking the drive loop; the pinned turns are A's pair, A's submit turn, B's pair, B's gated turn — dropping A's pair (the per-node merge the pre-fix code performed) fails the equality. |
| Phase A frontier review round 1 (finding 1), failure history on the early re-park: a drive loop that observes new failures before a sibling re-parks publishes the checkpoint's ORIGINAL history plus those failures — the same derivation the completion path's collector applies over the same known-failed set, stamped with the iteration the restored plan executes under; the next resume seeds its known-failed ids from the published plan, so a dropped failure is never recorded | the re-published checkpoint's failure history (field-exact) + the published plan's failed-node shape + the second resume's coordinator decision context (the failure line EXACTLY ONCE under the resumed iteration, no parked-iteration line, the failed and completed plan-state lines) + per-tool invocation counts across both resumes | `an_early_re_park_publishes_the_drive_loops_new_failures` (aura): resume 1 soft-fails node A (no submit_result) while node B re-parks; resume 2 approves the fresh call, drives B to completion over the cloned coordinator's request log. |
| Phase A frontier review round 1 (finding 2), pair retention, LOOP re-park: an awaiting node that completed in the drive loop keeps its pairs when the CONTINUATION re-parks (a never-started sibling's gated call parks the resumed run) | whole Parked turns array + blocking (location-normalized fresh entry) + invocation counts | `a_completed_nodes_pairs_ride_the_loop_re_park` (aura): the pinned turns are A's pair, A's submit turn, then the parking sibling's gated turn — the pre-fix code dropped every awaiting node's pairs at this exit. |
| Phase A frontier review round 1 (finding 3), wave ordering, mixed wave: a parked worker's snapshot-derived turns join their ORIGINATING wave's task-id merge at full wire fidelity — task 0's gated turn precedes task 1's completion turn, where the pre-fix append trailed every completed turn of every wave | whole Parked turns array + blocking (location-normalized fresh entry) | `parked_wave_turns_merge_into_their_wave_by_task_id` (aura): the sibling nodes sit in the plan in REVERSE id order, so the pin exercises the merge's sort rather than the workers' build order; A's pair and turns ride ahead of the wave. |
| Phase A frontier review round 1 (finding 3), wave ordering, follow-up wave: a later runnable wave (task 2, dependent on the completing task 1) stays after the whole earlier mixed wave — the parked task 0's turns included | whole Parked turns array + blocking (location-normalized fresh entry) | `a_followup_wave_stays_after_the_earlier_waves_parked_turns` (aura): the mixed-wave fixture plus the dependent sibling; the pinned order is A's pair, A's turn, task 0's gated turn, task 1's turn, task 2's turn. |

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
- The re-parked segment's turns are SEGMENT-scoped, not node-scoped
  (frontier round 1, findings 2-3): every decided pair consumed in the
  segment rides them — a completed node's pairs ahead of that node's own
  continuation turns, a re-parked node's pairs ahead of its gated
  assistant turn — and the continuation's worker turns join in wave
  order, a parked worker's snapshot-derived turns inside their
  originating wave's task-id merge. A fill that drops a consumed pair or
  appends parked turns outside their wave fails the frames and must be
  reconciled with the panel. Since F2 (the R6 natural-finish fill) the
  COMPLETED segment's turns are the run's natural
  turns only — the awaiting nodes' continuation turns, the coordinator
  loop's worker turns, and the coordinator's conversation tail — because
  the outcome packages live inside the rebuilt histories the continuations
  streamed from; the same fill that drops a completed-turn pair must be
  reconciled the same way.
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
  JSON-quoted wire form. Since the P45 stage-3 wiring, the prelude no longer
  swaps slots in place: the segment-wide preflight validates every awaiting
  node's prompt as the tool-result user message (a tool-result-less prompt
  refuses — `tool_result_less_prompt_refuses_at_preflight_before_any_tombstone`),
  and the total `rebuild_context` rebuilds the continuation context, so the
  rebuilt context also carries each decided call's assistant tool call —
  synthesized where the fixture's history never captured it — ahead of
  every tool result. The retired slot-swap helper (`replace_tool_result`)
  was deleted at Stage 7, with its unit frame.
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
  builder must normalize (no orphaned results, no missing calls).   The
  frames stayed red on the fold's replace-miss fatal until the Stage 3
  reconstruction wiring landed (this change's commit; all three flipped green
  unedited); the retired slot-swap helper was reaped at Stage 7.
- `run_segment_borrowed` consumes the `test_rig` worker-override queue for its
  continuation's worker builds and the coordinator-override queue for the
  resumed coordinator loop's build, so the segment frames run scripted
  models on both seats. (Written when the atomic `run_segment` was the
  entry; the seam moved at S6, the consumption contract is unchanged.)
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
  Permission denied (os error 13)`), and the segment preflight's
  prompt-shape refusal (`awaiting node 0: the parked snapshot's current
  prompt is not the tool-result message the park producers write` — the
  row that retired the fold's replace-miss fatal; see the manifest row
  above). A fill wording these differently flips the frames and
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
  file, so only a read-only leftover at the temp path itself faults the
  write.
- The Stage-6 frames script WORKER builds through the worker-override
  queue and (since F2's R1 ruling) the COORDINATOR build through its own
  dedicated coordinator-override queue in `test_rig` — take-once,
  process-global, consumed by `create_coordinator`'s cfg(test) prelude,
  built through the same toolset a live coordinator receives. The
  worker queue's contract is unchanged, resolving the earlier panel
  note (a fill routing the coordinator through the WORKER queue would
  have changed that queue's contract; the dedicated queue does not).
  The scripted coordinator's turns are deterministic (one
  respond_directly whose turn text is the answer), so completing
  frames pin the coordinator tail exactly; the request-level
  restoration pins (the checkpoint's coordinator conversation riding
  the continuation prompt) belong to the later wire-level R6 unit.
- The Stage-6 frames install their worker overrides per resume, in
  build order, under a drop guard (`OverrideDrain`) that drains the
  queue on scope exit, unwind included: while the frames were staged
  red, the pre-Stage-6 segment never built the sibling's or
  replacement's workers, and an undrained leak would ride the next
  consumer's first worker build (the queue is take-once and
  process-global). The guard stays for regressions: any mid-test panic
  skips an end-of-test drain, staged or not.
- The re-plan frame's continuation leg is deliberately disjunctive (a
  replacement or retried task's worker ran OR a final answer landed):
  the choice between re-planning and answering is the coordinator's,
  and the ruling demands the loop continue, not a particular
  continuation. Its decision-context pins (the plan-state line carrying
  the failed node; the failure-history line under the resumed
  iteration) are exact and carry the load; a stream-error failure
  faults the segment instead of soft-failing.

## The A1 id-channel frames (2026-09-25)

Nine frames pin Mike's A1 ruling (the DESIGN.md section of the same name).
They arrived red (e40468c6, repaired e7cbae7b) and turned green with the
fill (1ea05b70):

- `a1_resumed_gate_requested_publishes_on_the_fresh_request_id_channel`
  (goldens.rs): a resumed re-park's gate-entry `Requested` reaches the
  broker subscriber keyed on the fresh request id. (`Completed` is
  structurally suppressed on a pending 207 reply; its split is pinned at
  the route level.)
- `a1_reparked_row_keeps_the_run_owner_request_id` (goldens.rs): the
  stored row keeps the bridge's run-owner re-mint.
- `a1_resumed_segment_arms_and_closes_mcp_under_the_fresh_request_id`
  (goldens.rs): an `ArmProbingTool` samples the manager's armed request id
  at the first scripted tool's execution — a fill arming only inside
  `close_segment_mcp` fails — and the close key is checked after
  completion. Observation rides the `cfg(test)`-only
  `mcp::manager::a1_observation` seam (records keyed by manager address).
- `hitl::route::tests::a1_id_channel`: `park_armed_worker_ask_post_body_
  names_the_run_owner` and `notify_leg_post_body_names_the_run_owner` pin
  the derived wire stamp on both park-armed legs;
  `single_agent_ask_post_body_keeps_the_fresh_request_id`,
  `hold_ask_post_body_keeps_the_fresh_request_id_under_worker_scope`,
  `single_scope_park_armed_ask_post_body_keeps_the_fresh_request_id`, and
  `single_scope_notify_leg_post_body_keeps_the_fresh_request_id` pin the
  ruling's boundaries (the mint is scope-gated AND mode-gated).
