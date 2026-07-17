<!-- markdownlint-disable MD033 -->
# Typed context-fixture schema for the golden-frame test harness

- Status: **accepted**
- Deciders: Mike Shearer
- Date: 2026-07-12

Card: S2, adapter repo `docs/redesign/cards/s2-golden-frame-harness.md`
(cross-repo, cited by path). Design record:
[context_fixture/DESIGN.md](../../crates/aura/src/orchestration/context_fixture/DESIGN.md).

## Context and Problem Statement

The orchestration simplification epic consolidates and reshapes the code that
assembles the coordinator's and workers' LLM request envelopes. Every such card
has to prove it changed no behavior at the prompt-assembly seam before anything
builds on it. No safety net existed for that: the epic plan's blast-radius scan
recorded no snapshot tests of full request envelopes and no test that constructs
the coordinator thread end to end (the epic plan, adapter repo
`docs/redesign/plans/2026-07-11-orchestration-redesign.md`, Stage 2). The
coverage that did exist, `frame_validation_tests.rs`, was 1,992 lines of
substring assertions over fragments of the envelope, brittle and partial.

The epic needed a fixture schema and a snapshot corpus that render the COMPLETE
request envelope (system preamble, full message list, and serialized
tool-definition JSON) so that a refactor card can assert byte-for-byte that its
change is inert. The schema also had to be the artifact the user reviews and
signs off as the context shape the epic builds on (the schema Gate U).

## Decision Drivers <!-- optional -->

- A refactor card needs a falsifiable envelope-identity gate against the
  accepted baseline (`9df96382`).
- The user has to review one coherent type surface as the epic's context schema.

The corpus **MUST** render the complete request envelope for each covered call
(system preamble, full message list, serialized tool-definition JSON). Envelope
identity at the aura seam is a necessary condition for provider-request identity;
a partial envelope cannot carry that claim.

Fixture constructors **MUST** parse, not validate: a scenario that does not
correspond to a reachable baseline state must fail to construct. Each public
fixture type maps to one business rule and names the invalid state it forbids.

Builders **MUST** call the real production assembly functions; nothing
re-implements prompt text. A harness that restates the prompt tests itself, not
production.

Normalization **MUST** be location-anchored and fail loud on marker drift, so a
payload byte (a query, a tool description, a task result) that happens to contain
a normalization marker can never be silently rewritten.

Production visibility **MUST NOT** be widened for the harness. Seams the harness
needs are reached through `#[cfg(test)]`-gated delegating accessors.

A byte-identity assertion mode **SHOULD** exist for refactor cards and
**SHOULD** be proven by a no-op refactor and a one-byte negative control.

Every envelope-surface slot **SHOULD** be exercised by at least one fixture, and
a committed coverage manifest **SHOULD** name every surface and branch the corpus
covers and, explicitly, what it does not.

## Considered Options

- Typed context fixture plus `insta` snapshots of the complete envelope, with
  builders calling the real assembly functions and two location-anchored
  normalization passes.
- Keep and extend the `frame_validation_tests.rs` substring-assertion style.
- Record-and-replay of real provider requests captured from a live model loop.

## Decision Outcome

Chosen option: **typed context fixture plus complete-envelope `insta`
snapshots**. A `#[cfg(test)]` `context_fixture` module (renamed to
`golden_tests` for the corpus file at the user's request) carries a typed
scenario struct covering turn counts, prior results, artifacts, budgets, failure
history, and iteration state. Its builders call the real production assembly
functions through the `pub`, `pub(crate)`, and `#[cfg(test)]` accessor seams
inventoried in the design record. The snapshots render the coordinator initial
planning call, continuation calls at depths one through three, and worker calls
(frame populated, empty, and spilled; recon and non-recon; final-iteration
urgency).

Each public fixture type maps to one business rule and names the forbidden
invalid state: `PlanningBudget` cannot hold a zero budget or one that disagrees
with the roster config; `CompletedResultFixture` cannot carry an inline result
with a spill footer; `CoordinatorScenario` re-checks its cross-field states at
construction. The type inventory in the design record is the full list.

Normalization runs exactly two passes, both anchored per message before the
snapshot document is flattened: a timestamp scrub anchored at byte offset zero of
a user-message body, and a worker-order sort confined to the first user message.
An occurrence audit proves only the expected generated occurrences will be
touched and panics otherwise. Anything not matched by the two passes is compared
byte for byte.

The byte-identity mode runs the corpus with snapshot updating disabled
(`INSTA_UPDATE=no`). Its validity was shown both ways: a no-op refactor of the
planning-wrapper timestamp binding left every snapshot green, and a one-byte
change to the planning-wrapper tail failed exactly the coordinator snapshots
while the worker snapshots stayed green. Both transcripts are in the design
record.

The user set the standing design intent at the schema gate: snapshots are
complete, human- and agent-readable renders of the final state the model
receives, in the expect-test discipline; they mirror the current baseline
rendering with defects pinned, and roll forward only through intentional,
ledgered re-pins.

### Positive Consequences <!-- optional -->

- Every downstream refactor card gets a falsifiable envelope-identity gate
  against the accepted baseline, so a behavior change cannot pass as pure
  consolidation.
- Illegal fixtures do not compile: the type inventory forbids the
  production-unreachable states an adversarial design panel would otherwise have
  to catch by reading.
- The committed coverage manifest makes the corpus's blind spots explicit rather
  than implied, and the residual risks are named on the card.

### Negative Consequences <!-- optional -->

- The corpus infrastructure costs more test lines than the brittle assertions it
  subsumed: the fixed-boundary measurement came out at a net increase of about
  2,391 test `.rs` lines, and the anticipated reduction did not materialize on
  this card. The user accepted the deviation at the ratification gate and
  assigned the reduction target to the epic cumulative, where a later card
  deletes the brittle suite.
- Envelope identity at the aura seam is necessary but not sufficient for
  provider-request identity (the final request is assembled in a pinned rig fork)
  and is not by itself evidence of benchmark-score neutrality. Downstream cards
  must not read a passing snapshot gate as behavioral evidence (residual risk
  R1).
- Several surfaces stay outside the corpus and are named as gaps: MCP-sourced
  tool inventory content (R6), time-dependent behavior (R2), persistence and
  stream-event side effects (R4), and the escape-hatch stripped-preamble branch
  (R7).
- Some seams are not full production comparisons. The tool-registration-order
  gates are shape assertions, and the conversation-growth rule is only partially
  production-emitted (shared push helpers, test-side sequence). These are logged
  as R8 and were carried into S3's seam-closure scope.

## Pros and Cons of the Options <!-- optional -->

### Typed fixture plus complete-envelope snapshots

- Good, renders the whole envelope, so a refactor card can assert byte identity
  over the exact surface the model receives.
- Good, the parse-don't-validate constructors make production-unreachable
  scenarios fail to compile.
- Good, real assembly functions run under the snapshots, so the corpus tests
  production, not a restatement of it.
- Bad, the typed fixtures, seam accessors, and normalizer cost more lines than
  the assertions they replace.

### Extend the substring-assertion style

- Good, no new infrastructure, and the existing tests already run.
- Bad, substring assertions over envelope fragments cannot express whole-envelope
  identity, so a refactor that moves text between covered and uncovered regions
  passes silently.
- Bad, the suite was already 1,992 brittle lines; extending it deepens the
  problem the epic set out to reduce.

### Record-and-replay of live provider requests

- Good, captures the request at the true downstream boundary, past the rig fork.
- Bad, the capture depends on a live model loop and MCP-sourced inventory, so it
  is neither deterministic nor reproducible in a unit test.
- Bad, it gives no typed, reviewable schema for the user to sign off as the
  epic's context shape.

## Links <!-- optional -->

- Design record:
  [context_fixture/DESIGN.md](../../crates/aura/src/orchestration/context_fixture/DESIGN.md)
  (type inventory, envelope seam table, normalization design, residual risks, and
  the net-reduction measurement contract).
- Coverage manifest:
  [context_fixture/MANIFEST.md](../../crates/aura/src/orchestration/context_fixture/MANIFEST.md).
- Card S2, adapter repo `docs/redesign/cards/s2-golden-frame-harness.md`, commit
  range `9df96382..3136fe19`.
- Consumed by ADR
  [2026-07-15-unified-bounding-module](2026-07-15-unified-bounding-module.md),
  which uses this corpus as its consolidation gate.
- RFC 2119: <https://www.rfc-editor.org/rfc/rfc2119>
