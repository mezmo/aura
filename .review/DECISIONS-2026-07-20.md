# Decision record: #271 HITL Park/Reify planning session (2026-07-20)

Source of truth for the design decisions behind this board. Made with Mike in
the planning session that created the board; reviewed adversarially by codex
5.6-sol (14 findings, dispositions below). Cards cite this file instead of
restating it.

GitHub issue: mezmo/aura#271 "HITL Park/Reify".
Repo worktree: `aura-orchestration-mode/.claude/worktrees/271-hitl-park-reify-plan` (off origin/main bb813e54).

## Scope

- In: run-level FSM (types-first ADR), serializable orchestration state,
  blocked detection, durable park, claim/lease, headless reify,
  retrieval-by-handle, file-backed store, automated durability harness.
- Out: Redis backend (#325), auth/identity ADR (T1-D), coordinator-mediated
  approvals (#384), memory_dir lifecycle (LOG-23580), the `/aura` endpoint
  family (types shaped for it only), mid-worker conversation resume,
  object-storage artifacts (T2-F).

## Design decisions

1. **Quiescence rule.** A worker hitting an approval gate durably parks that
   approval; its task enters `TaskState::Blocked{decision_id}`. The DAG
   scheduler keeps dispatching tasks not transitively dependent on a blocked
   one. The run parks only when the ready frontier is empty, nothing is
   running, and at least one task is Blocked - always a drained wave
   boundary. Coordinator-blocked (future) parks immediately.
2. **No mid-worker resume in V1.** A Blocked task's worker attempt re-runs on
   reify with the decision available. Pre-gate tool calls may re-execute
   (documented limitation). The decision itself is consumed exactly once,
   bound by decision_id plus args-digest revalidation.
3. **Two-level FSM.** Durable run FSM (session store, schema-versioned):
   `Created -> Running -> Parked{reason, resume_point, parked_at, expires_at}`
   with reify back to Running; terminals Completed / Failed / Cancelled
   (Expired-vs-Failed{cause} vocabulary is a P1 decision). Every non-human
   exit from Parked denies (fail-closed). Task FSM: existing states plus
   `Blocked{decision_id}`, `Blocked -> Pending` on decision. Fine-grained
   phases (Routing, Planning, ExecutingWave, Synthesizing, Replanning) stay
   SSE-observability only. Invariant: a run may only durably exist Parked at
   a wave or iteration boundary.
4. **Continuation by handle, never transcript merge.** A request referencing
   the parked run (session/run id or approval id) gets server-authoritative
   history; client messages are new trailing turns only; optional prefix-hash
   validation rejects mismatches. A `/v1/chat/completions` call without the
   handle is a new conversation. Handles are bearer capabilities while auth
   is deferred (state this in the ADR).
5. **Claim is a CAS lease on the session, not the parked item.** Parked items
   are wake reasons (HITL decision now; A2A messages, schedules, monitors
   later) that trigger a claim attempt. Pull-and-claim by the pod that
   receives the approval; no push subscriber. Two agents never hold one
   session. Lease carries holder, heartbeat, expiry, and a monotonic fencing
   generation; all session mutations reject stale generations. The initial
   attended run claims the session too.
6. **Storage split.** Session store holds the run FSM record, lease,
   parked-approval records, coordinator conversation (structured rig
   Messages), Plan snapshot, original query, external chat history, budget
   and timing state, config fingerprint, and approved-call bindings. Worker
   traces, artifacts, and scratchpad stay on pod-local memory_dir.
7. **Atomic checkpoint.** Park is one versioned `RunCheckpoint` blob
   committed by a single CAS `Running -> Parked`. No scattered writes.
   Checkpoint embeds completed-task outputs needed by future DAG waves;
   reify refuses when the checkpoint holds pod-local refs it cannot resolve.
8. **Decisions are never destroyed.** Approval resolution persists a durable
   wake reason (today's `ApprovalStore::resolve` removes the entry - that
   changes). The park commit reconciles decisions that arrived during drain:
   continue instead of parking, or leave a durable wake.
9. **Approval dispatch FSM.** `Pending -> Resolved -> DispatchClaimed ->
   Executed | ExecutionUnknown`. Crash after claim is ExecutionUnknown,
   never silently retried. Canonical JSON hashing and digest-mismatch behavior are
   P1 type decisions. Exactly-once correlates with an existing board-identified
   need; correlate the GH ticket when project/1 reorganization settles.
10. **Request-scope teardown transfer.** `RequestResourceGuard::drop` deletes
    approvals when the SSE request ends. Parking transfers ownership from
    request scope to session scope before the response closes; a staged test
    proves park-induced closure keeps the approval.
11. **Typed outcomes through the stack.** `execute()` stringifies worker
    errors today, which would erase a park signal as Failed. New intermediate
    outcomes: `ToolAttemptOutcome::Blocked(ApprovalRef)`,
    `TaskExecutionOutcome`, `WaveOutcome`, `IterationOutcome::Parked`.
12. **Expiry.** Reuses the HITL timeout config surface; a durable
    reaper (scanner or durable timer, claim plus fencing, idempotent
    terminalization) owns unattended expiry - a live Tokio timer dies with
    the process. Run-level expiry denies outstanding approvals and terminates
    the run failed-with-summary. Task budget clocks suspend while Blocked.
13. **Identity headers.** Per-header TOML classification: `identity`
    (reified user-id headers behind a trusted gateway; persisted in the
    checkpoint, replayed on reify) vs `credential` (the default; unparkable
    in V1 - the gate refuses to park, fail-closed, naming the header).
    `CredentialSource` enum: StaticConfig | RequestForwarded |
    ServiceIdentity | BrokeredDelegation; last two are typed holes for the
    T1-D identity ADR. Matches the industry pattern (checkpoints store
    credential references, never secrets; unattended work runs under service
    identity or brokered grants; A2A agents authenticate as themselves).
14. **FileSessionStore.** A file-backed session store backend ships as a real
    OSS-quality backend (not test-only): preserves the zero-infra standalone
    constraint and lets the harness prove true process-restart durability.
    V1's claim is "backend-independent park/reify protocol"; cross-pod
    arrives with the Redis backend (#325).
15. **Retrieval is snapshot-authoritative in V1.** The Parked SSE frame emits
    only after the CAS succeeds. Event replay with cursors is future work.
16. **Session identity.** The ADR must reconcile Session-as-durable-UUID with
    today's client-supplied reused `chat_session_id`. Proposal going in: one
    non-terminal run per session; the handle names the session.
17. **Domain glossary.** Session: durable UUID identity (conversation,
    artifacts, FSM record); hydratable anywhere; executes nowhere. Agent
    (instance): reified execution environment on one pod, born by claiming a
    session, optional request_id (attended) and task_id (attended or
    unattended). Artifacts belong to sessions; turns belong to sessions;
    tasks have a session and an agent. Naming vs the existing `[agent]` TOML
    concept is a P1 decision. Mike's endpoint sketch (`/aura`: chat,
    list_session, load_session, kill, park, schedule, list_approvals) shapes
    the types but is not built in V1.

## Method

Rust typed holes: P1 designs types plus red unit tests; P2 wires them through
real call sites with `todo!()` bodies (compiles clippy-clean, tests red);
hole-fill cards make named red tests green without editing them (test edits
need logged Gate A justification). Acceptance layer is frame/expect-style
golden tests modeled on the orchestration-simplification golden frames
(terminalbench board). Board process per PROCESS.md; model bindings per
REVIEW-TOOLING.md and MODEL-CLASSES.md. P1 is authored by Fable with codex
5.6-sol as Gate A reviewer.

## Codex 5.6-sol review dispositions

Findings 1-5, 7-11, 14 absorbed as decisions 7-12, 15 and P1/P2 card
requirements. Finding 6 and 12 (cross-pod honesty) resolved by decision 7
and 14. Finding 13 (card decomposition) resolved by the card DAG on this
board (checkpoint store before blocked propagation before durable
resolution; expiry owned; reify and retrieval adjacent). Full review output
is preserved in the planning session; re-run `codex exec` against the ADR at
Gate A rather than citing the old output.

## Addendum 2026-07-25: redis session store merged (PR #393)

The redis session store merged to main (`e80386a9..86ece6ca`, main @
`18a37458`) after this record was written and after P1's branch base
(`bb813e54`). A 27-agent orchestrated investigation re-vetted every card
against merged main; the decisions below amend the record. Card scope
edits landed the same day (see each card's log).

A. **Scope rewording, not reversal.** "Out: Redis backend (#325)" now
   reads: redis backends for ApprovalStore, EventBus, and the A2A
   TaskStore are ON main; what #325 still owns is the redis
   implementation of the run-store surface this board designs (checkpoint,
   lease, run FSM). This board still ships no redis run store.
B. **The run-store surface is a new capability trait.** Main has no
   transaction/CAS/lease surface to extend: `ApprovalStore` is five
   methods, `EventBus` is fire-and-forget pub/sub, and the composite
   `SessionStore` in aura-web-server is a capability factory
   (approvals/tasks/bus/ping). P2 designs the checkpoint/lease/run-FSM
   trait fresh (working name `RunStore`; final name a P2 design decision -
   `SessionStore` is taken), exposed on the factory as an OPTIONAL
   capability with a defaulted absence model:
   `fn runs(&self) -> Option<Arc<dyn RunStore>>` returning `None` by
   default (a mandatory method would force a redis edit - the factory
   returns a type-erased trait object, so cargo features cannot gate it).
   Memory and file implement it in this board; redis inherits the `None`
   default and compiles untouched, preserving decision A. Required
   behavior: a redis-configured deployment without #325 REFUSES durable
   parking cleanly at the gate, naming the missing capability (fail
   closed, never silent fallback); P2 stages that test red.
C. **Backend vocabulary.** "Both backends" board-wide now means: memory
   and file implement the run-store surface; redis exists for today's
   capabilities and joins via #325. Conformance batteries name all three
   where the trait under test has three implementors (ApprovalStore does;
   RunStore does not yet).
D. **File-store selection is env-var, like the merged convention**
   (Mike, 2026-07-25): `AURA_SESSION_STORE=file` plus the URL variable
   interpreted as a path, a `SessionStoreBackend::File` variant in
   aura-config, a factory arm in aura-web-server, and env parsing wired
   into aura-cli standalone construction at its one PRODUCTION
   construction site (`direct.rs:110`; the second
   `InMemorySessionStore::new()` at `:468` is a `#[cfg(test)]` fixture
   and stays explicitly injected - never env-sensitive). Selection alone
   is not enough in standalone: construction must build the selected
   store once and thread `PendingApprovals::with_backend(approvals, bus)`
   from it, mirroring the web server - today standalone separately builds
   `PendingApprovals::new()`, which owns its own in-memory store and bus.
E. **Decision 8 has two write sites.** `resolve` is destructive on both
   live backends (memory map-remove; redis GETDEL discarding the decision
   value - PR #393 finding M1). At-most-once currently falls out of the
   deletion; P7 relocates it into the dispatch-FSM CAS and re-implements
   resolve on memory + file + redis. M1 gets a GH issue referencing
   #271-P7 documenting the interim 204-then-denial exposure.
F. **Decision 14's cross-pod deferral is revised.** In redis-configured
   multi-instance deployments, approval routing is cross-instance TODAY
   (any instance resolves; wake over pub/sub); the memory default and the
   file backend remain single-process, and their docs say so. What
   stays deferred is cross-pod run claim/reify, gated on the redis run
   store (#325) and shared artifact storage (aura#421, Archil at
   memory_dir). File-store lease semantics must stay correct if
   memory_dir becomes shared storage (#421): do not assume pod-locality
   for correctness, only for artifact resolvability.
G. **Serialization discipline reconciliation.** Main's merged wire rule is
   "domain types stay unserializable; `record.rs` projections are the only
   converter." P1's park types serde-derive directly (ratified at
   U(types)). P2 reconciles: either extend record.rs with run/checkpoint
   projections or add the ADR note justifying direct derives for run-FSM
   types (a checkpoint IS a wire format). Ships one discipline, not two.
H. **Request teardown is now two-phase.** RequestResourceGuard::drop does
   a sync local cancel plus a spawned async store/bus cleanup that may
   never poll at shutdown. Parking must disarm both halves before drop,
   atomically, and durable-park correctness must not depend on the async
   half. P2's staged red test covers the async-cleanup-races-park case.
I. **Findings ledger.** F1 (routed-cancel terminal loss): standalone GH
   issue after a rig reproduction (Mike: verification harness first);
   no card touches the A2A bridge; P11 carries a terminal-frame
   delivery check citing it. F3 (conformance battery): absorbed by P3
   (exported backend-parameterized battery) and P4 (backend matrix). M1:
   issue referencing #271-P7 after the same rig reproduction (see E).
   M3 (relay seq-gap): an expected-failure row on P11 tied to its
   disposition, never counted green. N-1: VERIFIED FIXED on main
   (`route.rs:110-120` registers before publishing; regression test at
   `:559-567`) - no action.
J. **Rebase posture.** card/271-P1 merges onto main @ `18a37458` with
   zero conflicts; all park/ imports resolve; no P1.5 card. P3 owns the
   rebase and `cargo check` as its opening checklist item - it runs
   first (P2 depends on P3 and starts from the post-P3 accepted head).
K. **Demo harness cards (P12, P14).** A watchable-state extension of the
   deterministic 2-instance rig. P12 (phase 1, no board deps): demos
   cross-instance approval and reproduces M1/F1 against merged main -
   the reproductions are the verification evidence Mike wants before the
   M1/F1 issues file. P14 (phase 2, depends P5/P7/P9/P12, gated S -> A):
   points the watcher at the run-store keys (checkpoint commit, lease
   CAS, reify) and is the acceptance-demo candidate for P11's Gate M;
   Gate M falls back to the P4 harness if P14 is not done by then.
L. **P3 split (codex round-1 blocker).** P3 as one card would implement
   the run-store trait P2 designs behind Mike's U(surface) gate, while
   claiming to run in parallel with P2. Split: P3 keeps the
   parallel-safe half (file ApprovalStore, config variant, factory arm,
   CLI standalone composition per D, tasks() posture, exported
   conformance battery over the traits that exist today);
   P13 (FileRunStore) implements the approved run-store trait after P2
   passes U(surface), and owns the CAS atomicity documentation. The
   shared factory file lands in P3 first; P2 depends on P3 (a full
   ordering edge, not merely serialize-with) and cuts its branch from
   the post-P3 accepted head. The durability rows of the battery move
   with P13.
M. **Expiry ownership consolidates on P7.** P7 defines the full approval
   retention policy (Pending, Resolved, Claimed, terminal, run-expiry) -
   including whether register-time TTL survives; P8 consumes and tests
   that policy; P6 only proves teardown ownership transfer (survival
   through both teardown halves before natural expiry; beyond-TTL
   survival is policy, P7's). The record-versioning and MIGRATION
   STRATEGY for changing the persistent redis record shape (versioned
   envelope or key namespace, old-reader/new-reader rules, mixed-version
   tests or a documented coordinated-shutdown migration) is a user
   decision: it is settled in the ADR amendment presented at P2's
   U(surface) gate, and P7 implements the approved strategy under its
   S -> A gates - this answers the "record versioning" open question
   from the PR #393 review.
N. **No escape clauses in acceptance.** Redis-gated checks run against
   the ephemeral-Valkey make target (`test-integration-session-store-local`
   pattern; requires only Docker) as a mandatory part of the relevant
   Gate S - "when a live redis is available" phrasing is void. A known
   gap is an explicit expected-failure row tied to a numbered issue and a
   user-approved deferral; it is never counted green and never replaced
   by a log entry.
