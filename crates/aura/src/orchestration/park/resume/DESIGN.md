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
| `evaluate.rs` | Presented-identity resolution, blocking entries (plain and non-empty), the refusal rows, the ordered evaluation stages, the grant, the segment seam |
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
| `IdentityHash` | The binding comparison is hash-to-hex, both sides produced by hashing/parsing 64 hex chars; comparison-only, never serialized | A raw header value stored or compared as if it were the hash; a credential proxy on the wire |
| `IdentityBindingState` | Binding off ⇒ no identity; binding on ⇒ a presented hash or a missing header, each distinct; resolved inside the evaluation from the bundle's raw inputs, never supplied | The `Option<IdentityHash>` ambiguity where "binding off" and "header absent" collapse; a resolved state contradicting its config's bind flag |
| `BlockingEntry` | One outstanding parked call renders `{decision_id, tool, expires_at}` | A blocking row without a typed decision id or expiry |
| `NonEmptyBlocking` | The `parked` and `expired` (pre-sweep) rows and the parked segment each carry at least one outstanding call; serializes as the plain JSON array | An empty `blocking` array where the spec promises entries |
| `ParkedToolName` | The tool label on a blocking row is its own type, not a bare string colliding with other strings | Cross-crate name collision with `aura::ToolName` (the broker event key) |
| `Diagnostic` | Human text that no caller branches on (rule-5 escape hatch) | Wire/control-flow decisions made on prose |
| `ConflictCode` | The six not-ready codes, declared in evaluation order | A code outside `running…parked`; out-of-order rendering |
| `ResumeConflictRow` | One 409 shape `{code, detail, blocking}` for every not-ready row | Six divergent 409 body shapes; a conflict row without its code |
| `ResumeRefusal` | The evaluation's refusal rows in card order; two detail-less rows, then conflicts, then faults | A verdict outside the table; a 404 row carrying detail |
| `ResumeEvaluation` | One bundle per request: path, the parked-dir input, config, store, claim table, the bind flag, the presented identity, request id, clock | A stage reading a request facet the bundle does not carry; documents or a resolved binding state constructed independently of their inputs |
| `ResumeGrant` | All-decided ⇒ authorization; the grant is the only door to `run_segment` | A segment running on an unevaluated or refused run |
| `SegmentTurns` | A segment — completed or parked — carries at least one turn | An empty `turns` array on the 200 body |
| `SegmentResult` | A segment ends completed or parked, never both | A success body with `blocking` on a completed segment |
| `SegmentError` | Mid-segment faults are distinct from refusals | A resume fault rendering as an evaluation verdict |
| `evaluate_resume` | First matching row wins; each stage's fault type admits only its own rows (row purity is structural); the stage sequence is the body's | A verdict from a foreign stage's fault type; sequence drift, which the Layer-2 golden tests pin |
| `run_segment` | One call = one segment; atomic data, no mid-segment streaming | Partial streaming of a segment's turns |

## How the evaluation order is encoded

`evaluate_resume` is a pipeline of private stages, one per row group:

`locate_checkpoint` (row 1) → `check_identity` (row 2) → `check_claim`
(row 3) → `admit` (rows 4–5, includes the rename-back) →
`check_fingerprint` (row 6) → `consult_decisions` (rows 7–9) →
`authorize` (row 10).

Each stage returns its own small fault enum (`LocateFault`,
`IdentityFault`, `AdmitFault`, `FingerprintFault`, `ConsultFault`, plus
`ClaimResumeFault` in `claim.rs`, shared by `check_claim` and
`authorize`), and each converts into `ResumeRefusal` through a private
`From` impl. A stage's fault type therefore admits only its own rows: row
purity is structural. The stage sequence itself is the body's — the types
do not pin it; the Layer-2 golden tests do. `ResumeRefusal`'s declaration
order mirrors the table for rendering.

`authorize` returns `ClaimResumeFault` directly (no second claim-failure
enum): its `From` impl maps `Live` — a claim lost to a concurrent
evaluation between `check_claim` and `authorize` — to
`ResumeRefusal::Conflict(ResumeConflictRow::running())`, so a lost claim
race renders 409 `running`, never the 500 fault sink.

Two duties ride on specific stages:

- **Config-changed enforcement** is `check_fingerprint` running before
  `consult_decisions`: rejecting before the recorded-decisions consult means
  zero tool invocations and no consumed-decision cleanup, structurally.
- **Redis discrimination** is `consult_decisions`: a missing ticket past the
  document's `expires_at` is the expired row (a remote TTL had swept it), a
  missing ticket inside the window is the mismatch row. `project_blocking`
  re-derives the outstanding set from the document so the expired row can
  carry the pre-sweep blocking list; it returns `NonEmptyBlocking`, so an
  empty re-derivation faults loudly instead of rendering an empty body.

## Recorded readings

- **Blocking population.** Blocking entries are consult-derived only: the
  `parked` row, and the `expired` row's pre-sweep list. The read-side
  refusal rows (`running`, `interrupted`, `config_changed`, `mismatch`)
  render an empty list. Justification: a polling client cannot act on
  approvals for a run it cannot claim, and the card's acceptance exercises
  blocking only on `parked` and `expired`. Revisit-able at U(endpoint) if
  the broad reading (populating from the document on the read-side rows) is
  wanted.
- **Cross-call precedence.** The consult resolves first-hit-in-call-order:
  the first pending call whose row fires decides the verdict, even where a
  later call would have fired a different row. The ruled exception: an
  expired redis ticket is deleted by the remote TTL, so a missing ticket
  past `expires_at` reads expired, not mismatch.
- **Expired row's side effect.** `evaluate_resume` owns the expired row's
  unlink-checkpoint and sweep-approvals side effect, post-consult,
  pre-render: `project_blocking` has produced the pre-sweep blocking list
  by then, and the sweep (`park::commit::cancel_run_approvals`) publishes
  its broker events under the bundle's `request_id`.
- **Parked-arm turns.** `SegmentResult::Parked` carries `SegmentTurns`
  (non-empty) like the completed arm: the continuation's first gated
  assistant turn always exists, so a zero-turn park is unreachable in
  production. If ever observed it surfaces as a loud fault (residual risks
  below), never as a silent empty body.

## Fill-unit duties (in-crate; applied by the evaluate fill)

- **Thread the injected clock into `load_recorded_decisions`.** The helper
  read `chrono::Utc::now()` internally, so the expired, mismatch, and
  parked rows would have ignored the bundle's `now`; the fill unit threads
  `now` through the consult, and the helper's other callers (its own tests,
  the commit test, the orchestrator loop test) pass `Utc::now()` where they
  hold no clock.
- **Wrap `RehydrateError`'s raw payloads in `Diagnostic` at the consult
  boundary.** `Mismatch(String)`, `Store(String)`, and `Document(String)`
  predate the card; `consult_decisions` converts them where they enter
  `ConsultFault`, so no raw string crosses into a wire-relevant row.

## Visibility / seam table

| Seam | Visibility | Consumer |
| --- | --- | --- |
| `park::resume` re-exports | `pub use` at `orchestration/mod.rs` | `aura-web-server` (`aura::orchestration::*`) |
| Payload types reachable through re-exported carriers | `pub use` at both hops (`park::resume`, `orchestration`) | `aura-web-server`, which must be able to name every type in a re-exported signature: `Diagnostic` (`ResumeRefusal::Fault`), `ParkedToolName` (`BlockingEntry::tool`), `SegmentTurns`/`EmptySegment` (`SegmentResult`, `SegmentTurns::try_new`), `NonEmptyBlocking`/`EmptyBlocking` (`SegmentResult::Parked`, `try_new`), `SegmentError` (`run_segment`). Checked and excluded — named in no re-exported signature: `IdentityHash` (comparison-only since the binding state went private), `ResumeLease` (private `ResumeGrant` field), `IdentityBindingState` (private) |
| Stage fns, `LocatedCheckpoint`, the `*Fault` enums (private to `evaluate.rs`; `ClaimResumeFault` in `claim.rs`) | private / `pub(crate)` | the fill unit only |
| `ResumeClaimTable::is_live`, `rename_back_to_parked`, `claim_and_resume` | `pub(crate)` | in-crate evaluation stages |
| `ResumeDocuments::parked`/`resuming`, `Diagnostic::new`, `IdentityHash::from_stored`, `ParkedToolName::new`, `ResumeConflictRow` row constructors | `pub(crate)` | in-crate stages and the handler-side projections |
| `config_fingerprint`, `parked_document_dir` | `pub(crate)` in `park::commit`, reached as `super::super::commit::*` | `check_fingerprint`, `ResumeDocuments::for_path` |
| `RecordedDecisions`, `ParkedRun` | `pub(crate)` types behind private `ResumeGrant` fields | the in-crate segment runner; opaque to the web server |
| `ResumeGrant::session_id`/`run_id` | `pub` accessors | the endpoint's 200 body |
| `ResumeRunResponse`, `ResumeRunState`, `refusal_response`, `continuation_turns`, `from_segment` | private in `handlers.rs` under per-item `#[allow(dead_code)]` | wired when the handler body lands |
| Test-only accessors | none added | existing `#[cfg(test)]` accessors in the park module are untouched |

The `#![allow(dead_code)]` at `resume/mod.rs` covers exactly the new module;
the per-item `#[allow(dead_code)]` markers in `handlers.rs` cover exactly
the five not-yet-wired projections. Existing allows in `continuation.rs`
(:51, :124, :202), `document.rs` (:226), and `park/mod.rs` (:15) are
untouched, per the card.

## Residual risks

- **Identity-hash write side is unplumbed.** `ParkedRun.identity_hash` is
  read by the evaluation, but `build_document` writes `None`: populating it
  at park commit requires an identity hash on `ParkCommitInputs`, whose
  construction lives in `orchestrator.rs` — outside this card's scope.
  Until it lands, `bind_identity = true` fails every resume of a
  None-hash document closed (404 row), which is safe but useless.
- **Expired row's blocking list is a re-derivation, not the consult's
  output.** `load_recorded_decisions` returns `RehydrateError::Expired`
  without the outstanding set; `project_blocking` re-derives it from the
  document pre-sweep and returns `NonEmptyBlocking`, so an empty
  re-derivation — an expired row with nothing outstanding — surfaces as a
  500 fault. If that fault is ever observed, the expired row's
  reachability assumption is wrong; stop and re-derive the row's semantics.
- **A zero-turn park is a loud fault, by design.** The continuation's first
  gated assistant turn always exists, so `SegmentTurns::try_new` cannot
  reject a real park; a re-park carrying zero turns fails the segment
  (500) rather than rendering an empty `turns` array.
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
- **`RehydrateError` still carries raw `String` payloads** for its
  non-resume consumers. The resume consult wraps them in `Diagnostic` at
  the boundary (duty above), so prose never crosses into a wire-relevant
  row untyped; elsewhere the raw strings remain the known escape hatch.
- **Anchor drift found:** the card's orientation says the chat-completion
  message types live under `streaming/`; they live in
  `aura-web-server/src/types.rs` (`ChatMessage`, `ChatMessageToolCall`,
  `ChatMessageFunctionCall`, `Role`). The reuse instruction is honored —
  the `turns` projection targets `crate::types::ChatMessage` — but the
  location note is stale.

## Hole inventory (`grep -rn 'todo!('` over new/changed code)

| Location | Hole |
| --- | --- |
| `park/resume/claim.rs` | `ResumeClaimTable::rename_back_to_parked` — filled |
| `park/resume/claim.rs` | `ResumeClaimTable::claim_and_resume` — filled |
| `park/resume/evaluate.rs` | `locate_checkpoint` — filled |
| `park/resume/evaluate.rs` | `check_identity` — filled |
| `park/resume/evaluate.rs` | `check_claim` — filled |
| `park/resume/evaluate.rs` | `admit` — filled |
| `park/resume/evaluate.rs` | `check_fingerprint` — filled |
| `park/resume/evaluate.rs` | `consult_decisions` — filled |
| `park/resume/evaluate.rs` | `project_blocking` — filled |
| `park/resume/evaluate.rs` | `authorize` — filled |
| `park/resume/evaluate.rs` | `evaluate_resume` — filled |
| `park/resume/evaluate.rs:744` | `run_segment` |
| `aura-config/src/config.rs:587` | `require_identity_header_for_binding` |
| `aura-web-server/src/server.rs:312` | `refuse_park_on_memory_backend` |
| `aura-web-server/src/handlers.rs:1403` | `continuation_turns` |
| `aura-web-server/src/handlers.rs:1441` | `resume_run` (handler dispatch) |
