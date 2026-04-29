# Aura Spec-Driven Development Process

This document defines the development lifecycle for new features in the Aura project. It uses GitHub spec-kit conventions with multi-agent review gates at every stage.

## Overview

Every feature follows an 8-phase lifecycle. Each phase has an explicit entry criterion, work product, and exit criterion. The process ensures traceability from roadmap items through user stories, specs, code, tests, and documentation.

```
Roadmap Item → User Stories → Specs → Review → Plan → Implement → Review → QA → Docs → Release
```

## Phase 1: Roadmap to User Stories

**Entry:** Gap identified in production readiness, user feedback, or strategic initiative.

**Work:**
- Write a roadmap item document in `.specify/roadmap/AURA-RM-NNN-<slug>.md`
- Include: rationale, user stories with Given-When-Then acceptance criteria, dependencies, affected crates, complexity

**Exit:** Product owner approves user stories.

## Phase 2: Three Specs

For each roadmap item, three specification documents are authored in `.specify/specs/AURA-RM-NNN/`:

### Product Spec (`product-spec.md`)
- What to build
- User stories with acceptance criteria (Given-When-Then)
- API/config contracts (exact shapes)
- Scope and non-scope
- Dependencies on other roadmap items

### Architecture Spec (`architecture-spec.md`)
- How to build it
- Constitution compliance check
- Crate changes (which modules, types, traits)
- Data flow diagrams
- Error handling approach
- Migration/backward compatibility plan
- Alternatives considered with rationale

### Quality Spec (`quality-spec.md`)
- How to verify it
- Test strategy (unit, integration, e2e)
- Acceptance criteria traceability matrix: every AC maps to test cases
- Test infrastructure needed
- Quality gates checklist

Templates for all three are in `.specify/templates/`.

## Phase 3: Multi-Agent Spec Review

Each spec goes through review by multiple agents, each with a distinct perspective.

### Review Perspectives

| Agent | Focus | Checks |
|-------|-------|--------|
| **Consistency** | Constitution + patterns | API compat? Crate boundaries? Config backward-compat? |
| **Architecture** | Technical design | Simpler alternatives? Concurrency concerns? Error handling? |
| **Security** | Threat surface | Auth bypass? Input validation? Secret handling? DoS vectors? |
| **Operability** | SRE concerns | Observable? Configurable without restart? Failure modes clear? Graceful shutdown? |
| **Testing** | Quality spec | All ACs covered? Edge cases identified? Test infra realistic? |

### Feedback Categories

- **Must Fix** — Blocks proceeding. Factual errors, spec violations, missing critical concerns.
- **Should Fix** — Strongly recommended. Design improvements, better patterns available.
- **Nit** — Optional. Style, naming, documentation phrasing.

### Exit Criteria

**The review comes back CLEAN: zero Must Fix AND zero Should Fix.**

Nits are at author discretion. The cycle repeats until clean:

1. Author addresses all Must Fix items
2. Author addresses all Should Fix items (fix or justify with rationale)
3. Re-review focuses only on changed sections
4. Loop until clean

### Review Records

Each review round is saved in `.specify/specs/AURA-RM-NNN/reviews/` with:
- Reviewer perspective
- Findings by category (Must Fix, Should Fix, Nit)
- Resolution status

## Phase 4: Plan + Tasks

**Entry:** All three specs approved (clean review).

**Work:**
- `plan.md` — Implementation plan derived from architecture spec
- `tasks.md` — Ordered task list with:
  - Traceability: each task references `Satisfies: AC-NNN.N.N`
  - Dependencies: never build against functionality that doesn't exist yet
  - Size: each task is one PR

**Exit:** Tasks approved, ready for implementation.

## Phase 5: Implementation + Multi-Agent Code Review

Each task becomes a PR. The PR goes through multi-agent code review.

### Code Review Perspectives

| Agent | Focus |
|-------|-------|
| **Correctness** | Does code match architecture spec? Error paths handled? No panics in production? |
| **Performance** | Unnecessary allocations? Async correct? Lock contention? |
| **Testing** | Tests satisfy mapped ACs from quality spec? Edge cases covered? |
| **Style** | Follows existing Aura patterns? Naming? Module organization? |

### Exit Criteria

Same protocol: **zero Must Fix AND zero Should Fix** before merge.

## Phase 6: QA Verification

**Entry:** All implementation tasks merged, tests passing.

**Work:**
- Run acceptance criteria from product spec against the implementation
- For API endpoints: automated integration tests (Docker Compose infrastructure)
- For config changes: validate with `debug_config` binary
- For UI components: Playwright tests against a running instance
- **Traceability check:** every acceptance criterion in the product spec has at least one passing test

**Exit:** All ACs verified. Traceability matrix complete.

## Phase 7: Documentation

**Entry:** QA verification complete, all acceptance criteria passing.

**Work:**
- Update `README.md` with new features, configuration options, and usage examples
- Update relevant docs/ files (streaming-api-guide, request-lifecycle, etc.)
- Add or update PromQL examples, config snippets, or API contract changes
- Update integration test feature flag list if new flags were added
- Update CHANGELOG.md with the feature summary
- Review all documentation changes for accuracy against the implemented code

**Exit:** All user-facing documentation reflects the current implementation. A new user could discover and use the feature from documentation alone.

## Phase 8: Validation + Release

- `cargo fmt --check` — formatting clean
- `cargo clippy --all-targets --all-features -- -D warnings` — linter clean
- `cargo test --workspace --lib` — all tests pass
- Integration test suite with new feature flags
- Backward compatibility: existing example configs still load
- No secrets in diff
- Squash or clean commit history for PR
- Conventional commit format with proper footer
- Semantic release via Jenkins pipeline

## Traceability

Every requirement flows through with traceable links:

```
AURA-RM-NNN (Roadmap Item)
  → US-NNN.N (User Story)
    → product-spec.md AC-NNN.N.N (Acceptance Criterion)
      → architecture-spec.md (Design Decision)
        → tasks.md: Task N (Implementation Task)
          → PR #NN (Code Change)
            → test_function_name() (Test)
              → quality-spec.md TC-NNN.N.N (maps back to AC)
```

Maintained by convention:
- Each task in `tasks.md` includes `Satisfies: AC-NNN.N.N`
- Each test function references its test case ID from the quality spec
- PRs reference the task and roadmap item in the commit message

## Execution Order

Roadmap items are executed strictly by dependency. Never start an item until its dependencies are complete — no speculative coding against unbuilt functionality.

```
RM-008 (Error Taxonomy) ← foundation, no dependencies
  ├── RM-001 (Metrics)
  │     ├── RM-005 (Token Budget)
  │     ├── RM-003 (Circuit Breaker)
  │     │     └── RM-011 (Admin Endpoint)
  │     └── RM-011 (Admin Endpoint)
  ├── RM-002 (Health Checks)
  ├── RM-004 (Auth)
  └── RM-007 (Incident Response)

Independent:
  RM-006 (CLI) — in progress on separate branch
  RM-009 (Agent Catalog)
  RM-010 (Runbook Scaffolding)
```

## Tools

- **Spec-kit CLI:** `specify` (installed via `uv tool install specify-cli`)
- **Test generation:** `make test-generate` (Aura self-testing pipeline)
- **CI:** Jenkins (commitlint, fmt, test, clippy)
- **Integration tests:** Docker Compose (`make test-integration`)
