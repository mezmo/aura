---
id: P6
title: Implement blocked propagation with wave drain and quiescence parking
status: in-progress
depends: [P5, P13]
serialize-with: [P9]
lineage: accepted-head
executor: smart
gates: "S -> A"
user-gates: []
---

# P6: Implement blocked propagation with wave drain and quiescence parking

Mechanics: [PROCESS.md](../PROCESS.md). Required reading: decisions 1-3,
8, 11, 12 in [DECISIONS-2026-07-20.md](../DECISIONS-2026-07-20.md).

## Scope

- `crates/aura/src/orchestration/orchestrator.rs` (`execute()`,
  `execute_task`, loop outcome threading),
  `crates/aura/src/orchestration/types.rs` (TaskState::Blocked wiring),
  `crates/aura/src/hitl/gate.rs` (Blocked outcome instead of held await
  when durable park is configured).

## Deliverable

The quiescence rule live: gate hit yields `ToolAttemptOutcome::Blocked`,
task enters Blocked with its budget clock suspended, the scheduler keeps
dispatching independent tasks, and when the frontier is empty with
nothing running the run commits its checkpoint (P5) and parks. Decisions
that arrive during drain are reconciled at the park commit (continue, do
not park). Park-induced SSE closure keeps the approval (the P2 staged
test goes green).

## Acceptance

- Named P1/P2 red tests and the P4 park frames green without edits.
- A drain-race test proves an approval landing mid-drain results in
  continuation, never a stranded park.
- The park commit uses the registry's local-only teardown path
  (`cancel_request_local`), never the store-deleting `cancel_request`,
  and disarms both teardown halves per addendum H. The ownership-transfer
  test runs against redis via the ephemeral-Valkey target (mandatory,
  addendum N) and proves a parked run's approval survives both teardown
  halves BEFORE its natural expiry. Survival beyond the register-time
  TTL is retention policy, which is P7's to define and test (addendum
  M); this card only proves the transfer.
- Runs without HITL configuration are behaviorally unchanged (existing
  suite green).

## Gate checklist

- [ ] Gate S: fmt, clippy, `cargo test --workspace`, harness park frames,
      `make test-integration-session-store-local` (the ownership-transfer
      redis row; mandatory, addendum N).
- [ ] Gate A: cross-family review focused on the drain race and on Blocked
      never collapsing into Failed.

## Branch

`card/271-P6` off accepted head. serialize-with P9 (shared orchestrator.rs).

## Log

- 2026-07-20 Card created from the planning session.
- 2026-07-25 Re-vetted against merged main (redis session store, PR #393): acceptance extended with the local-only teardown requirement (registry now splits cancel_request_local from the store-deleting cancel_request), the two-phase Drop disarm (addendum H), and the redis approval-record TTL hazard for parked runs.
- 2026-07-25 Codex round-1 fixes: the "or log why" escape clause removed (addendum N) - the TTL-window test is mandatory via ephemeral Valkey; retention-policy ownership moved wholly to P7 (addendum M).
- 2026-08-05 Promoted backlog -> ready by the board owner (GLM-5.2-Fast, OpenCode session): both deps (P5, P13) done. P13 done at `0ff75695`; cuts from accepted head.
- 2026-08-05 Promoted ready -> in-progress by the board owner. Branch `card/271-P6` cut from accepted head `0ff75695` (P13's tip) into worktree `.claude/worktrees/271-P6`. Executor: rust-write subagent (Kimi K2.7-Code, smart class). Running in parallel with P7 (WIP 2).
