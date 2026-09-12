# P45 skeleton design record — the resume endpoint's type surface

Layer 1 typed-holes unit: real signatures, real derives, `todo!()` bodies for
everything with behavior. The public surface lives in
`orchestration/park/resume/` and is re-exported flat from
`aura::orchestration`; the wire projections live in
`aura-web-server/src/handlers.rs`.

## Module map

| File | Concern |
| --- | --- |
| `claim.rs` | Path-segment parse types, checkpoint document locations, the per-run claim table and its lease |
| `evaluate.rs` | Identity binding, blocking entries, the refusal rows, the ordered evaluation stages, the grant, the segment seam |
| `mod.rs` | Re-exports; the `#![allow(dead_code)]` slice for the skeleton |

## Type → business rule → forbidden invalid state

| Type | Business rule | Invalid state made unrepresentable |
| --- | --- | --- |
| `ResumeSessionId` | A session path segment must be a safe single path component before any filesystem access | A session id containing `/`, `\`, `..`, or empty cannot be constructed |
| `ResumeRunId` | A run path segment must parse as a UUID and be a safe path component | A non-UUID or unsafe run id cannot reach the document layer |
| `ValidatedResumePath` | Both path segments are validated as one step, before any read | A partially-validated request (one segment checked) has no type |
| `ResumeDocuments` | A run's checkpoints are exactly two names — `{run}.json` and `{run}.resuming.json` — under one session's parked dir | Ad-hoc path arithmetic per call site; a name derived from an unvalidated segment |
| `ResumeClaimTable` | At most one live in-process resume claim per run | Two simultaneous claims for one run inside this process |
| `ResumeLease` | A segment's claim lives exactly as long as the holder's scope | A claim that outlives its segment (released on drop) |
| `MalformedId` | Malformed path segments answer 404 before any read; the reason is diagnostic-only | A caller branching on the rejection reason |
| `IdentityHash` | The binding comparison is hash-to-hex, both sides produced by hashing/parsing 64 hex chars | A raw header value stored or compared as if it were the hash |
| `IdentityBindingState` | Binding off ⇒ no identity; binding on ⇒ a presented hash or a missing header, each distinct | The `Option<IdentityHash>` ambiguity where "binding off" and "header absent" collapse |
| `BlockingEntry` | One outstanding parked call renders `{decision_id, tool, expires_at}` | A blocking row without a typed decision id or expiry |
| `ParkedToolName` | The tool label on a blocking row is its own type, not a bare string colliding with other strings | Cross-crate name collision with `aura::ToolName` (the broker event key) |
| `Diagnostic` | Human text that no caller branches on (rule-5 escape hatch) | Wire/control-flow decisions made on prose |
| `ConflictCode` | The six not-ready codes, declared in evaluation order | A code outside `running…parked`; out-of-order rendering |
| `ResumeConflictRow` | One 409 shape `{code, detail, blocking}` for every not-ready row | Six divergent 409 body shapes; a conflict row without its code |
| `ResumeRefusal` | The evaluation's refusal rows in card order; two detail-less rows, then conflicts, then faults | A verdict outside the table; a 404 row carrying detail |
| `ResumeEvaluation` | One bundle per request: path, documents, config, store, claim table, identity, clock | A stage reading a request facet the bundle does not carry |
| `ResumeGrant` | All-decided ⇒ authorization; the grant is the only door to `run_segment` | A segment running on an unevaluated or refused run |
| `SegmentTurns` | A segment — completed or parked — carries at least one turn | An empty `turns` array on the 200 body |
| `SegmentResult` | A segment ends completed or parked, never both | A success body with `blocking` on a completed segment |
| `SegmentError` | Mid-segment faults are distinct from refusals | A resume fault rendering as an evaluation verdict |
| `evaluate_resume` | First matching row wins; row order is the stage pipeline's order | Out-of-order evaluation: each stage's error type admits only its own rows, so stage N cannot emit stage M's verdict |
| `run_segment` | One call = one segment; atomic data, no mid-segment streaming | Partial streaming of a segment's turns |

## How the evaluation order is encoded

`evaluate_resume` is a pipeline of private stages, one per row group:

`locate_checkpoint` (row 1) → `check_identity` (row 2) → `check_claim`
(row 3) → `admit` (rows 4–5, includes the rename-back) →
`check_fingerprint` (row 6) → `consult_decisions` (rows 7–9) →
`authorize` (row 10).

Each stage returns its own small fault enum (`LocateFault`,
`IdentityFault`, `ClaimFault`, `AdmitFault`, `FingerprintFault`,
`ConsultFault`), and each converts into `ResumeRefusal` through a private
`From` impl. A stage therefore cannot produce another row's verdict — the
order is structural, not a comment. `ResumeRefusal`'s declaration order
mirrors the table for rendering.

Two duties ride on specific stages:

- **Config-changed enforcement** is `check_fingerprint` running before
  `consult_decisions`: rejecting before the recorded-decisions consult means
  zero tool invocations and no consumed-decision cleanup, structurally.
- **Redis discrimination** is `consult_decisions`: a missing ticket past the
  document's `expires_at` is the expired row (a remote TTL had swept it), a
  missing ticket inside the window is the mismatch row. `project_blocking`
  re-derives the outstanding set from the document so the expired row can
  carry the pre-sweep blocking list.

## Visibility / seam table

| Seam | Visibility | Consumer |
| --- | --- | --- |
| `park::resume` re-exports | `pub use` at `orchestration/mod.rs` | `aura-web-server` (`aura::orchestration::*`) |
| Stage fns, `LocatedCheckpoint`, `*Fault` enums | private to `evaluate.rs` | the fill unit only |
| `ResumeClaimTable::is_live`, `rename_back_to_parked`, `claim_and_resume` | `pub(crate)` | in-crate evaluation stages |
| `ResumeDocuments::parked`/`resuming`, `Diagnostic::new`, `IdentityHash::from_stored`, `ParkedToolName::new`, `ResumeConflictRow` row constructors | `pub(crate)` | in-crate stages and the handler-side projections |
| `config_fingerprint`, `parked_document_dir` | `pub(crate)` in `park::commit`, reached as `super::super::commit::*` | `check_fingerprint`, `ResumeDocuments::for_path` |
| `RecordedDecisions`, `ParkedRun` | `pub(crate)` types behind private `ResumeGrant` fields | the in-crate segment runner; opaque to the web server |
| `ResumeGrant::session_id`/`run_id` | `pub` accessors | the endpoint's 200 body |
| `ResumeRunResponse`, `ResumeRunState`, `refusal_response`, `continuation_turns`, `from_segment` | private in `handlers.rs` under per-item `#[allow(dead_code)]` | wired when the handler body lands |
| Test-only accessors | none added | existing `#[cfg(test)]` accessors in the park module are untouched |

The `#![allow(dead_code)]` at `resume/mod.rs` covers exactly the new module;
the per-item `#[allow(dead_code)]` markers in `handlers.rs` cover exactly
the four not-yet-wired projections. Existing allows in `continuation.rs`
(:51, :124, :202), `document.rs` (:226), and `park/mod.rs` (:15) are
untouched, per the card.

## Residual risks

- **Identity-hash write side is unplumbed.** `ParkedRun.identity_hash` is
  read by the evaluation, but `build_document` writes `None`: populating it
  at park commit requires an identity hash on `ParkCommitInputs`, whose
  construction lives in `orchestrator.rs` — outside this card's scope.
  Until it lands, `bind_identity = true` fails every resume of a
  None-hash document closed (404 row), which is safe but useless.
- **Expired row's blocking list needs a pre-sweep re-derivation.**
  `load_recorded_decisions` returns `RehydrateError::Expired` without the
  outstanding set; the fill unit must project the blocking list before the
  unlink-and-sweep, or re-derive it from the document.
- **Claim-lock discipline is a fill-unit obligation.** The table uses a
  `std::sync::Mutex` so `ResumeLease::drop` can release without a runtime.
  The rename-under-lock holes must acquire the guard inside a
  `spawn_blocking` closure (never hold a std guard across `.await`).
  `claim_and_resume`'s atomicity (insert + rename under one lock) is the
  hardest body in the unit; the golden frames should include a concurrent
  claim-vs-rename interleaving.
- **`IdentityHash` comparison is not constant-time.** The hash is a
  credential proxy; if the threat model includes a timing oracle over the
  resume endpoint, swap the derived `PartialEq` for a constant-time compare
  in the fill unit.
- **`load_recorded_decisions` returns raw `String` diagnostics**
  (`Mismatch(String)`). `ResumeConflictRow::mismatch` wraps them in
  `Diagnostic`, which is fine for the wire, but the underlying variant
  still invites branching on prose elsewhere.
- **Anchor drift found:** the card's orientation says the chat-completion
  message types live under `streaming/`; they live in
  `aura-web-server/src/types.rs` (`ChatMessage`, `ChatMessageToolCall`,
  `ChatMessageFunctionCall`, `Role`). The reuse instruction is honored —
  the `turns` projection targets `crate::types::ChatMessage` — but the
  location note is stale.

## Hole inventory (`grep -rn 'todo!('` over new/changed code)

| Location | Hole |
| --- | --- |
| `park/resume/claim.rs:189` | `ResumeClaimTable::rename_back_to_parked` |
| `park/resume/claim.rs:199` | `ResumeClaimTable::claim_and_resume` |
| `park/resume/evaluate.rs:367` | `locate_checkpoint` |
| `park/resume/evaluate.rs:376` | `check_identity` |
| `park/resume/evaluate.rs:382` | `check_claim` |
| `park/resume/evaluate.rs:393` | `admit` |
| `park/resume/evaluate.rs:402` | `check_fingerprint` |
| `park/resume/evaluate.rs:414` | `consult_decisions` |
| `park/resume/evaluate.rs:424` | `project_blocking` |
| `park/resume/evaluate.rs:437` | `authorize` |
| `park/resume/evaluate.rs:446` | `evaluate_resume` |
| `park/resume/evaluate.rs:503` | `run_segment` |
| `aura-config/src/config.rs:587` | `require_identity_header_for_binding` |
| `aura-web-server/src/server.rs:312` | `refuse_park_on_memory_backend` |
| `aura-web-server/src/handlers.rs:1401` | `continuation_turns` |
| `aura-web-server/src/handlers.rs:1439` | `resume_run` (handler dispatch) |
