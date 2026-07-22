# S46 named-check type design record

Baseline: aura `7a0f0651`, branch `card/S46`. Scope: the named-check mechanism.
Phase 1 landed the type skeleton; phase 2 landed the bodies — render,
render-site reconciliation, the acceptance predicate, the worker-to-render
bridge, the template wording, and the OPTION-IN tool-schema advertisement. The
design of record is the U(mockup) packet v3
(`docs/redesign/2026-07-21-s46-enforcement-mockups.md`), sections 2, 7, and 8;
the gate decisions this record implements are OPTION-IN (round 4) and the
task-side declaration in-card (round 5).

The mechanism has two ends. A task MAY name the check that decides its
success at plan creation, and the worker's result carries that check's
decisive outcome at `submit_result`. The evidence frame reconciles the two
(a declared check the worker did not carry renders `NOT RUN`), so completion
can no longer be accepted on a self-report while the deciding check sits
unasked. The two ends are modeled by two types on two sides: the bounded
`CheckIdentity` on `Task.named_check_declaration` (the task side), and the
`NamedCheck` worker-side evidence (the worker side). `NamedCheck` itself is
single-ended - it carries only the worker's evidence, not the declaration -
so this record does not call the type "two-ended".

## Type inventory

Every new public type maps to one business rule and names the invalid state it
forbids. Types marked (extended) are pre-existing types this card adds a field
to; (wire) types are the serde boundary the model fills, parsed into the
bounded domain types downstream.

| Type | Business rule | Forbidden invalid state |
|---|---|---|
| `CheckIdentity` (`named_check.rs`) | The identity of the verification a task's success depends on - what is checked and its criterion. Parse-don't-validate: bounded and non-empty at construction | An empty identity; an identity over `NamedCheckWidth::DEFAULT` (a decisive check line cannot swell into a paragraph) |
| `CheckResult` (`named_check.rs`) | The decisive result a check produced (a count, a delta, a pass/fail line) that settles pass or fail | An empty result; a result over `NamedCheckWidth::DEFAULT` (bulk transcript is unrepresentable here - it must reference the spilled artifact, packet §7) |
| `CheckOutcome` (`named_check.rs`) | A named check produced a decisive result (`Performed`), the worker engaged it but could not complete it and carried an observation (`Incapable`), or it was not run (`NotRun`); `NOT RUN` is explicit. `Incapable` and `NotRun` are split so worker-reported incapacity-with-observation never aliases reconciled-absent (design-panel P5) | `NOT RUN` encoded as an empty/blank `CheckResult` string (it is a variant, not a sentinel value; packet §8); a worker incapacity observation collapsed into `NotRun` and lost |
| `NamedCheck` (`named_check.rs`) | The worker-side evidence: the check identity paired with its outcome (one end of the two-ended mechanism; the task-side declaration is `Task.named_check_declaration`). Identity is non-optional, so a decisive result never travels divorced from the check it belongs to | A decisive result with no identity; a declared check silently absent from a result (absence is `CheckOutcome::NotRun`, never "no line") |
| `NamedCheckWidth` (`bounding.rs`) | The named-check field is bounded to one decisive line that survives spill. The module's other char widths truncate; this one **rejects**, because a decisive datum silently cut is worse than absent | A zero cap; (by consumers) silent truncation of a decisive result |
| `NamedCheckArgs` (`submit_result.rs`, wire) | Raw worker submission: the check performed (`result`), or what the worker `observed` when it could not; both absent records the worker named the check but carried nothing. The `observed` slot keeps an incapacity note structured rather than in free-form self-report (design-panel P5); it is advertised in the tool schema alongside `named_check` (OPTION-IN, round 4; advertisement landed in phase 2) | n/a (wire; validated on parse into `NamedCheck`) |
| `SubmitResultArgs.named_check` (extended, wire) | The worker declares the decisive check's evidence in the structured tool call, not only inside free-form `result` (OPTION-IN) | n/a (wire) |
| `SubmitResultOutput.named_check: SubmittedCheck` (extended) | The stored worker result carries what the submission held for the decisive check, the acceptance gate's data source - separate from the free-form `result` that spills. `SubmittedCheck` is a three-state enum (`Absent`, `Present(NamedCheck)`, `UnrepresentableIdentity`) so a rejected submission never aliases "no check named" (design-panel RV1) | The gate reading a self-report `summary` in place of the check; a submission whose identity was unrepresentable collapsing into `Absent` |
| `SubmittedCheck` (`submit_result.rs`) | What a worker's submission carried for the decisive check: absent, a bounded `Present(NamedCheck)`, or a check submitted with an unrepresentable identity | An unrepresentable-identity submission dropped to "no check named" (the two are distinct variants, not one `None`) |
| `WorkerClaim.named_check: Option<NamedCheck>` (extended) | The claim is the stand-in that survives result spill (`ArtifactStandIn::Claim`); the worker-attested decisive check rides on it (design A, packet §7), and fixtures read it from here. The production render path draws the check line from the reconciled per-entry value (`CompletedEntry` / `PriorWorkEntry` `named_check`), not from this field | (render-time) a spilled decisive check lost behind the artifact pointer - the reconciled check renders per-entry on both the inline and spill paths while the worker-attested value rides the claim |
| `StepInput::LeafTask.named_check: Option<String>` (extended, wire) | A task MAY name the check that decides its success at plan creation (the task-side leg of the mechanism, gate round 5) | n/a (wire; `Option`; most tasks name none) |
| `Task.named_check_declaration: Option<CheckIdentity>` (extended) | The task's declared check travels from plan creation to the evidence frame as a bounded domain value (parsed in `flatten_one`, not raw wire text), where absence in the worker's result is reconciled to `NOT RUN` (design-panel P1: the wire-to-domain bound is not bypassed on the task leg) | A declared check that cannot reach the acceptance/render site; an empty or over-bound declaration reaching reconciliation as an unrepresentable string (rejected at flatten instead) |

## Visibility / seam table

No production visibility was widened. The new module is `mod named_check`
(private to `context`), re-exported through the `context` facade. The domain
types are constructed only through their parsing constructors; raw text enters
at the wire boundary and leaves as bounded values.

| Item | Visibility | Decision |
|---|---|---|
| `named_check` module | `mod` (private in `context`), types re-exported via `context::{CheckIdentity, CheckOutcome, CheckResult, NamedCheck}` | Mirrors the other `context` submodules; the domain types are the crate-facing surface |
| `NamedCheckWidth` (`bounding.rs`) | `pub` in the private `bounding` module | Reachable from `context` (a descendant of `orchestration`); co-located with every other bound per the module's "one source of truth" charter |
| `CheckIdentity::new`, `CheckResult::new`, `NamedCheck::parse`, `NamedCheck::not_run` | `pub`, return `Result<_, ContextError>` | Parse-don't-validate; consumers obtain a bounded value through these constructors |
| `serde(try_from = "String")` on `CheckIdentity` / `CheckResult` | - | Deserialization runs the bounded constructor, so a persisted or wire value cannot bypass the bound |
| `NamedCheck::render_line`, `NamedCheck::reconcile`, `declared_check_satisfied` | `pub` | Phase-2 render, render-site reconciliation, and the acceptance predicate. `reconcile` reads `SubmittedCheck` from `tools`, closing the same `context -> tools` loop `Confidence` already uses |
| `WorkerClaim::with_named_check` | `pub`, additive builder | Existing `WorkerClaim::new` callers are untouched; the field defaults to `None` |
| `submit_result` (tools) → `context::NamedCheck` | - | Adds a `tools -> context` reference closing the loop `context -> tools` already opened for `Confidence`; intra-crate module cycles are permitted and carry no type cycle |

## Phase 2 (landed)

- **Reconciliation.** `NamedCheck::reconcile(declaration, submitted)` turns a
  declared-but-uncarried check into `CheckOutcome::NotRun` at the render site,
  keyed off `Task.named_check_declaration` (design-panel P4). It is
  declaration-driven: no declared check renders no line (the negative-space
  clause), except an unrepresentable submission, which is surfaced not dropped
  (RV1).
- **Render.** `NamedCheck::render_line` emits `[Check: {identity} -> {outcome}]`
  (`Performed` result, `COULD NOT PERFORM: {observed}`, or `NOT RUN`).
  `CompletedEntry` and `PriorWorkEntry` each carry the reconciled
  `Option<NamedCheck>` and append the line after the evidence, on every entry
  shape — so a declared check stays visible through result spill and into the
  worker-to-worker frame (packet section 8 Views 1-4).
- **The worker → render bridge (R2 resolved).** `SubmittedCheck` threads as one
  owned source (design-panel P7): `SubmitResultOutput.named_check` ->
  `StructuredTaskOutput.named_check` -> `WorkerClaim` (via `TryFrom`, design A
  carrier) and, at the render site, into the reconciled entry value. No third
  parallel optional was added; `StructuredTaskOutput` gained the one field,
  `skip_serializing_if` on the checkless path so legacy JSON stays identical.
- **Acceptance obligation (P2).** `declared_check_satisfied(declaration,
  submitted)` requires identity equality AND `CheckOutcome::Performed`;
  everything else (incapable, not-run, mismatch, unrepresentable, absent) is
  unverified and surfaces as the reconciled `NOT RUN` / `COULD NOT PERFORM`
  render line in the coordinator's view. The control model is unchanged: no
  runtime hard-reject gate was added (bounded router stays).
- **Schema advertisement.** `submit_result` now advertises `named_check`
  (`check` / `result` / `observed`) in its JSON parameter schema (OPTION-IN,
  round 4). Tool-definition snapshots move with the schema (intended).

## Still deferred (phase-2 out of scope)

- **Deterministic enforcement gate.** The harder target — reject a
  `submit_result` that omits `named_check` when the task declared a check,
  without consuming first-write state so the worker retries — stays recorded in
  the ledger (R5) and unbuilt; it would change the tool's error type.

## Residual risks (named)

- **R1 - the render carrier choice, resolved by the design panel (ledger
  P4).** The decisive check rides on `WorkerClaim` (design A) rather than on a
  new field of `CompletedEntry` / `PriorWorkEntry` (design B). Design A carries
  the check through both the inline (`InlineResult.claim`) and spill
  (`ArtifactStandIn::Claim`) paths for free and matches packet §7's
  "next to the stand-in" argument, at far lower blast radius. Its gap: an
  entry with no `WorkerClaim` (a claimless `InlineResult`, a `Preview`
  stand-in, or `ArtifactPointerOnly`) has no carrier, so a declared check on a
  claimless result cannot render `NOT RUN`. This is a legibility gap, not an
  enforcement hole: the deterministic gate reads `SubmitResultOutput.named_check`
  plus the task declaration, which do not depend on a claim. **Panel ruling:**
  design A stands for the committed phase-1 skeleton - the worker-attested
  value rides the claim. The per-entry gap is not left open: the panel bound a
  phase-2 requirement that the *reconciled* check render on every entry shape,
  landing the reconciled value per-entry at reconciliation time. That delivers
  design B's per-entry legibility at the layer that owns rendering while
  keeping the claim as the worker-attested carrier, so packet §8 View 4's
  prior-work legibility is preserved.
- **R2 - resolved (phase 2): the worker-to-render bridge is wired.**
  `SubmittedCheck` threads `SubmitResultOutput.named_check` ->
  `StructuredTaskOutput.named_check` -> `WorkerClaim` and the reconciled entry
  value, one owned source (P7). `StructuredTaskOutput` gained the single field
  (the anticipated ~12 construction sites, mostly tests), with
  `skip_serializing_if` keeping checkless stored output byte-identical.
- **R3 - the field bound value.** `NamedCheckWidth::DEFAULT` is 200 characters -
  a placeholder honoring the "one decisive line" intent; identity and result
  share one cap. The exact value (and whether identity should be capped tighter
  than result) is a design-panel tunable, not evidence-derived.
- **R4 - resolved (design-panel P5): incapacity observation is structured.**
  Packet §5c's incapacity clause asks a worker that cannot perform a check to
  "report what you did observe." That observation now rides on
  `CheckOutcome::Incapable(CheckResult)`, split from `NotRun` (reconciled-absent)
  so the two provenances never alias. The wire carries it in
  `NamedCheckArgs.observed`, advertised in the tool schema alongside
  `named_check` (OPTION-IN, round 4; the advertisement landed in phase 2).
- **R5 - resolved (design-panel P3, extended by RV1): rejected evidence is
  preserved, never aliased to absent.** `submit_result`'s `call` still has
  `type Error = Infallible`, and the stored `named_check` is a three-state
  `SubmittedCheck`. When the worker's carried *result* fails the field bound but
  the *identity* holds, the identity is preserved as
  `SubmittedCheck::Present(NamedCheck)` with `CheckOutcome::NotRun`, so the
  deciding datum's absence stays visible. When the *identity itself* is empty or
  over-bound - so no bounded `NamedCheck` can be built at all - the submission
  is recorded as `SubmittedCheck::UnrepresentableIdentity`, still distinct from
  `Absent`: a rejected submission never masquerades as "no check named" (RV1).
  The harder phase-2 target - hard-reject the submission without consuming
  first-write state so the worker can retry with bounded evidence - is recorded
  in the ledger and not built here; it would change the tool's error type.
- **R6 - no type encodes semantic decisiveness (design-panel P8).** The bound
  keeps a `CheckResult` to one line and forbids blank sentinels, but nothing at
  the type level distinguishes a decisive datum (`VIOLATION: g00000
  has 53`) from a plausible-looking summary sentence that type-checks as a
  `Performed` result. The guarantee that the carried result is the *actual*
  deciding output rests partly on model compliance, plus the S45 grounding
  dimension applied at measurement time - not on this type. Recorded as honest
  residual risk; no phase-2 type is proposed to close it.
