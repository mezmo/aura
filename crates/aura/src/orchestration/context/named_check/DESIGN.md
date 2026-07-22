# S46 named-check type design record

Baseline: aura `7a0f0651`, branch `card/S46`. Scope: the type skeleton for
the two-ended named check (phase 1 of the S46 type discipline). Bodies where
behavior is nontrivial are `todo!()`; plain field additions are complete. The
design of record is the U(mockup) packet v3
(`docs/redesign/2026-07-21-s46-enforcement-mockups.md`), sections 2, 7, and 8;
the gate decisions this record implements are OPTION-IN (round 4) and the
task-side declaration in-card (round 5).

The mechanism has two ends. A task MAY name the check that decides its
success at plan creation, and the worker's result carries that check's
decisive outcome at `submit_result`. The evidence frame reconciles the two
(a declared check the worker did not carry renders `NOT RUN`), so completion
can no longer be accepted on a self-report while the deciding check sits
unasked.

## Type inventory

Every new public type maps to one business rule and names the invalid state it
forbids. Types marked (extended) are pre-existing types this card adds a field
to; (wire) types are the serde boundary the model fills, parsed into the
bounded domain types downstream.

| Type | Business rule | Forbidden invalid state |
|---|---|---|
| `CheckIdentity` (`named_check.rs`) | The identity of the verification a task's success depends on - what is checked and its criterion. Parse-don't-validate: bounded and non-empty at construction | An empty identity; an identity over `NamedCheckWidth::DEFAULT` (a decisive check line cannot swell into a paragraph) |
| `CheckResult` (`named_check.rs`) | The decisive result a check produced (a count, a delta, an exit line) that settles pass or fail | An empty result; a result over `NamedCheckWidth::DEFAULT` (bulk transcript is unrepresentable here - it must reference the spilled artifact, packet §7) |
| `CheckOutcome` (`named_check.rs`) | A named check either produced a decisive result (`Performed`) or was not run (`NotRun`); `NOT RUN` is explicit | `NOT RUN` encoded as an empty/blank `CheckResult` string (it is a variant, not a sentinel value; packet §8) |
| `NamedCheck` (`named_check.rs`) | The two-ended evidence: the check identity paired with its outcome. Identity is non-optional, so a decisive result never travels divorced from the check it belongs to | A decisive result with no identity; a declared check silently absent from a result (absence is `CheckOutcome::NotRun`, never "no line") |
| `NamedCheckWidth` (`bounding.rs`) | The named-check field is bounded to one decisive line that survives spill. The module's other char widths truncate; this one **rejects**, because a decisive datum silently cut is worse than absent | A zero cap; (by consumers) silent truncation of a decisive result |
| `NamedCheckArgs` (`submit_result.rs`, wire) | Raw worker submission: the check performed and the result it produced; `result` absent records the worker named the check but could not perform it | n/a (wire; validated on parse into `NamedCheck`) |
| `SubmitResultArgs.named_check` (extended, wire) | The worker declares the decisive check's evidence in the structured tool call, not only inside free-form `result` (OPTION-IN) | n/a (wire) |
| `SubmitResultOutput.named_check: Option<NamedCheck>` (extended) | The stored worker result carries the bounded decisive check, the acceptance gate's data source - separate from the free-form `result` that spills | The gate reading a self-report `summary` in place of the check (the field is a distinct, bounded slot) |
| `WorkerClaim.named_check: Option<NamedCheck>` (extended) | The claim is the stand-in that survives result spill (`ArtifactStandIn::Claim`), so it is the render carrier that keeps the decisive check in the coordinator's view when the bulk result spills to an artifact (packet §7) | (render-time) a spilled decisive check lost behind the artifact pointer - the check rides on the claim, which renders inline on both the inline and spill paths |
| `StepInput::LeafTask.named_check: Option<String>` (extended, wire) | A task MAY name the check that decides its success at plan creation (the task-side leg of the mechanism, gate round 5) | n/a (wire; `Option`; most tasks name none) |
| `Task.named_check_declaration: Option<String>` (extended) | The task's declared check travels from plan creation to the evidence frame, where absence in the worker's result is reconciled to `NOT RUN` | A declared check that cannot reach the acceptance/render site (it is carried on the task, beside `structured_output`) |

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
| `NamedCheck::render_line` | `pub`, `todo!()` | Phase-2 render body; the `[Check: {identity} -> {outcome}]` line. Unused in phase 1 (no caller), so existing renders and golden snapshots are unchanged |
| `WorkerClaim::with_named_check` | `pub`, additive builder | Existing `WorkerClaim::new` callers are untouched; the field defaults to `None` |
| `submit_result` (tools) → `context::NamedCheck` | - | Adds a `tools -> context` reference closing the loop `context -> tools` already opened for `Confidence`; intra-crate module cycles are permitted and carry no type cycle |

## Phase boundary (what phase 1 does NOT do)

- **Reconciliation.** Turning a declared-but-absent check into
  `CheckOutcome::NotRun` at the render/acceptance site is a `todo!()`-class
  body deferred to phase 2. Phase 1 provides the types
  (`NamedCheck::not_run`, `CheckOutcome::NotRun`) and the carriers.
- **Render.** `NamedCheck::render_line` and the wiring that appends the
  `[Check: ...]` line into `CompletedEntry::render` / `PriorWorkEntry::render`
  are phase 2. Existing render bodies are unchanged, so no golden snapshot
  moves.
- **The worker → render bridge.** `SubmitResultOutput.named_check` reaches the
  continuation-prompt render only once the orchestrator threads it onto the
  task's stored output and builds the reconciled `WorkerClaim`. Phase 1 stops
  at the field additions; the threading (and any consequent field on
  `StructuredTaskOutput`) is phase-2 wiring, flagged in R2 below.
- **Enforcement gate and schema advertisement.** The deterministic
  "reject a `submit_result` that omits `named_check` when the task declared a
  check" gate, and advertising `named_check` in the tool's JSON parameter
  schema, are phase-2 behavior. The `submit_result` JSON schema is left
  byte-identical so tool-definition snapshots do not move.

## Residual risks (named)

- **R1 - the render carrier choice, and the claimless gap.** The decisive
  check rides on `WorkerClaim` (design A) rather than on a new field of
  `CompletedEntry` / `PriorWorkEntry` (design B). Design A carries the check
  through both the inline (`InlineResult.claim`) and spill
  (`ArtifactStandIn::Claim`) paths for free and matches packet §7's
  "next to the stand-in" argument, at far lower blast radius. Its gap: an
  entry with no `WorkerClaim` (a claimless `InlineResult`, a `Preview`
  stand-in, or `ArtifactPointerOnly`) has no carrier, so a declared check on a
  claimless result cannot render `NOT RUN`. This is a legibility gap, not an
  enforcement hole: the deterministic gate reads `SubmitResultOutput.named_check`
  plus the task declaration, which do not depend on a claim. **Open question
  for the design panel:** accept design A with this gap, or pay design B's
  blast radius for a per-entry reconciled `Option<NamedCheck>` that renders
  `NOT RUN` on every entry.
- **R2 - the worker-to-render bridge is unwired.** `SubmitResultOutput` carries
  the worker's `named_check`, but the path onto `Task`/`WorkerClaim` at the
  render site is phase 2. It may require a `named_check` field on
  `StructuredTaskOutput` (~12 construction sites, mostly tests) - deferred so
  phase 1 stays a minimal type surface. Flagged so the panel weighs the
  eventual ripple.
- **R3 - the field bound value.** `NamedCheckWidth::DEFAULT` is 200 characters -
  a placeholder honoring the "one decisive line" intent; identity and result
  share one cap. The exact value (and whether identity should be capped tighter
  than result) is a design-panel tunable, not evidence-derived.
- **R4 - `NotRun` carries no observation.** Packet §5c's incapacity clause asks
  a worker that cannot perform a check to "report what you did observe."
  `CheckOutcome::NotRun` is payload-free in phase 1; a future
  `NotRun { observed: ... }` could carry the incapacity note. Left minimal;
  flagged.
- **R5 - over-bound named_check is dropped, not rejected.** `submit_result`'s
  `call` has `type Error = Infallible`, so an over-bound or malformed
  `named_check` is logged (`warn`) and dropped to `None`, mirroring the
  existing confidence fallback. Making it a hard tool error would change the
  tool's error type - out of scope for a type skeleton. **Open question:**
  should an over-bound decisive check fail the tool call rather than degrade to
  absent?
