<!-- markdownlint-disable MD033 -->
# Unified bounding module for truncate, summarize, and spill decisions

- Status: **accepted**
- Deciders: Mike Shearer
- Date: 2026-07-15

Card: S3, adapter repo `docs/redesign/cards/s3-unified-bounding.md` (cross-repo,
cited by path). Design record:
[bounding/DESIGN.md](../../crates/aura/src/orchestration/bounding/DESIGN.md).

## Context and Problem Statement

Aura's orchestration code made every truncate, summarize, and spill decision at
the call site. The S3 inventory found 28 such sites spread across
four mechanisms that did not share a vocabulary: byte-based artifact spill,
token-based scratchpad budget, byte-based observability and manifest caps, and
ad-hoc `safe_truncate` and `truncate_query` literals for plan content and log
previews (s3-unified-bounding.md card log; the mechanisms are catalogued in
[bounding/DESIGN.md](../../crates/aura/src/orchestration/bounding/DESIGN.md)).
The same config value drove two surfaces in some places and a bare `usize` width
crossed a byte-versus-character boundary in others, so a reader could not tell
which unit a limit was in.

The epic wanted one typed source of truth for these decisions. The constraint
was that this is a pure consolidation: the change had to preserve byte-identical
output on every surface the golden-frame corpus covers (S3 card acceptance,
adapter repo `docs/redesign/cards/s3-unified-bounding.md`). The corpus from ADR
[2026-07-12-context-fixture-schema](2026-07-12-context-fixture-schema.md) is what
made that constraint checkable.

## Decision Drivers <!-- optional -->

- One vocabulary for bounding decisions, so a reader and a future card find every
  truncate/summarize/spill rule in one typed place.
- The consolidation must not smuggle in a behavior change under cover of a
  refactor.

Consolidation **MUST** preserve byte-identical output on every golden-frame
manifest surface. A card that cannot keep envelope identity is misfiled and moves
to the behavior-change track (Track B).

The typed config **MUST** represent every config value production accepts today,
including the boundary cases: zero thresholds used as sentinels, a duplicate-call
nudge threshold at or above the block threshold, `max_tools_per_worker = 0`, and
a summary width wider than its spill threshold. Rejecting any of these would be a
behavior change, not a consolidation.

Decision boundaries **MUST NOT** erase units. A byte-bounded surface and a
character-bounded surface must be distinguishable at the type level, so the two
mechanisms cannot be mixed at a call site.

The fail-open spill defect **MUST NOT** be fixed here. `maybe_create_artifact`
returning the full unbounded result when persistence fails to write the artifact
is a behavior change; it is recorded for a later card (S14).

Illegal threshold orderings **SHOULD** be unrepresentable where they are truly
illegal, through private newtypes, but the config **SHOULD NOT** reject an
ordering production reaches today.

## Considered Options

- One `BoundingConfig` typed budget built from `OrchestrationConfig`, with a
  per-surface width newtype for each bounded field, and production call sites
  rewired to consume it.
- Leave the 28 sites in place and document them in one inventory.
- A single generic truncate helper taking a bare `usize` width.

## Decision Outcome

Chosen option: **one `BoundingConfig` typed budget with per-surface newtypes**.
`BoundingConfig::from_orchestration` reads the `OrchestrationConfig` fields and
constructs the typed budget; each bounded surface gets its own newtype
(`ResultSpillBudget`, `ToolOutputSpillBudget`, `ToolListLimit`,
`DuplicateCallPolicy`, `SessionHistoryLimit`, and the byte and character width
types grouped into `LogPreviewWidths`, `ManifestWidths`, and
`PlanContentWidths`). Byte widths and character widths are distinct private
primitives (`ByteWidth`, `CharWidth`), so a call site cannot mix them. The method
bodies reproduce exact production behavior, including a label that prints
"chars" while measuring bytes, preserved byte-identically rather than corrected
here.

The type-discipline panel drove two repair rounds before implementation. The
first skeleton rejected production-reachable configs and introduced phantom
states and error variants; the repairs made the boundary cases representable,
removed a phantom `NudgeOnly` policy and the `BoundingError` type entirely, and
moved invalid threshold orderings behind private newtypes that are matchable but
not constructable with an invalid order.

The module deliberately does not absorb the two token budgets. The prior-work
frame's `TokenBudget` (a four-character-per-token heuristic) and the scratchpad
`ContextBudget` (a real tokenizer) stay owned by their modules. The design record
narrows the centralization claim to say so (residual risks R4 and R7).

Production call sites were wired to the typed config in a second phase, and the
same phase closed three seams the golden-frame harness had deferred (the R3
worker-preamble append order and the R8 conversation-growth and
tool-registration surfaces), to the adjudicated state recorded on the card.

### Positive Consequences <!-- optional -->

- Every bounding decision has one typed home, so a future card changes a limit in
  one place and the type tells it which unit the limit is in.
- The golden-frame corpus held byte-identical across the whole consolidation,
  which is the evidence that the refactor is inert.
- The type-discipline panel caught the boundary-rejection behavior change at the
  skeleton stage, before any body was implemented.

### Negative Consequences <!-- optional -->

- The consolidation did not reduce line count: the product-plus-template delta
  came out positive, and at the S3 landing commit (`3f75a68f`) the module
  carried 16 `#[allow(dead_code)]` items on its API surface awaiting a future
  seam, both flagged for a later retirement card. Later-card seam consumption
  and the S6 dead-code sweep have since drawn that count down.
- The byte-versus-character unit mismatch is preserved, not resolved:
  `safe_truncate` still operates on UTF-8 bytes while several config fields and
  comments describe characters. Aligning the two is a behavior change (residual
  risk R1).
- The token budgets remain uncolocated and use two different approximation
  methods, so "one source of truth" holds for byte and character bounding but not
  for token bounding (R4, R7).
- Three new required serde fields on `IterationContext` landed without
  `#[serde(default)]`; no production persistence path exercises them today, but a
  Gate A reviewer flagged the omission as a latent hazard, a candidate for S14.
- The config-load boundary now models the sentinel and misordered cases as valid;
  any future tightening of them is itself a behavior change requiring its own card
  (R6).

## Pros and Cons of the Options <!-- optional -->

### One `BoundingConfig` with per-surface newtypes

- Good, one typed vocabulary for every truncate/summarize/spill decision, built
  once from the orchestration config.
- Good, byte and character widths are distinct types, so the unit-mixing class of
  bug is unrepresentable at a call site.
- Good, the consolidation is checkable: the golden-frame corpus proves byte
  identity across the byte- and character-bounded sites it rewired (the two
  token-budget surfaces are deliberately left uncolocated, per the decision
  outcome above).
- Bad, the typed surface adds lines rather than removing them, and leaves dead-code
  API awaiting later seams.

### Leave the sites in place, document them

- Good, zero code change and zero risk to envelope identity.
- Bad, the decisions stay scattered across four mechanisms, so the next card that
  touches a limit still has to find every sibling by hand.
- Bad, the unit-erasure hazard (bare `usize` widths crossing byte and character
  boundaries) is left in place.

### A single generic truncate helper

- Good, the fewest new types, one function for every site.
- Bad, a bare `usize` width erases the byte-versus-character distinction, which is
  exactly the mixing hazard the drivers forbid.
- Bad, it cannot express the per-surface semantics (sentinel zeros, spill versus
  promote, marker styles) without branching that puts the scatter back inside the
  helper.

## Links <!-- optional -->

- Design record:
  [bounding/DESIGN.md](../../crates/aura/src/orchestration/bounding/DESIGN.md)
  (type inventory, visibility and seam table, consolidation inventory, residual
  risks R1 through R10).
- Card S3, adapter repo `docs/redesign/cards/s3-unified-bounding.md`, commit
  range `3136fe19..3f75a68f`.
- Depends on ADR
  [2026-07-12-context-fixture-schema](2026-07-12-context-fixture-schema.md): the
  golden-frame corpus is this card's consolidation gate.
- The fail-open spill defect and the `IterationContext` serde hazard are recorded
  for card S14, not fixed here.
- RFC 2119: <https://www.rfc-editor.org/rfc/rfc2119>
