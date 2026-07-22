<!-- markdownlint-disable MD033 -->
# Durable park and reify for orchestration runs

- Status: **proposed**
- Deciders: Mike Shearer
- Date: 2026-07-21

Technical Story: [#271](https://github.com/mezmo/aura/issues/271) (HITL Park/Reify)

## Context and Problem Statement

HITL V1 parks an approval in-process for exactly as long as the SSE stream that
carried the prompt stays open. A dropped stream denies the call, and an
orchestration run that hits an approval gate cannot outlive its request. The
session-storage ADR ([2026-07-08](2026-07-08-session-storage.md)) made the
parked-approval *record* durable behind `ApprovalStore`, but deliberately
excluded whole-run parking: the coordinator conversation, the plan DAG, and
worker state have no durable form, so there is nothing a later request could
resume. That gap is the largest unowned piece of the state-management picture.

This decision defines how an orchestration run parks durably at an approval
gate, how a later decision claims and reifies it (including on a fresh process
after a restart), and the domain vocabulary the implementation is built from.

## Decision Drivers <!-- optional -->

- The HITL fail-closed guarantee **MUST** survive parking: every non-human exit
  from a parked state (expiry, storage failure, malformed wake) denies the
  gated call and terminates the run as failed, never as approved.
- Park **MUST** be atomic. A crash during park leaves either a running run or a
  fully parked one; no reader may observe a partially written checkpoint.
- Two agent processes **MUST NOT** concurrently own one session. A stale owner
  resuming after a network partition **MUST NOT** be able to corrupt state a
  newer owner has already advanced.
- An approval decision **MUST** be consumed at most once, and only for the
  exact tool arguments the human saw.
- A resolved decision **MUST NOT** be destroyed before the run has consumed it,
  including decisions that arrive while the run is still draining toward its
  park point.
- The zero-infrastructure standalone deployment **MUST** keep working. Durable
  parking cannot require a networked store.
- Restart durability **MUST** be provable by an automated harness: park, kill
  the process, reify in a new one.
- Live runtime handles (oneshot senders, cancellation tokens, broadcast
  channels) **MUST NOT** be persisted; they are re-established on reify, per
  the session-storage ADR.
- V1 **SHOULD NOT** attempt mid-worker resume. Re-running a blocked worker
  attempt with the decision available is acceptable; revisit if pre-gate tool
  re-execution proves costly in practice.

## Considered Options

Each cluster lists the candidates that were weighed; the chosen one is named in the
Decision Outcome.

- **Park point**: park the whole run immediately when any worker blocks;
  checkpoint mid-worker (serialize the worker's conversation); or drain to a
  quiescent wave boundary and park there.
- **Continuation shape**: merge a client-supplied transcript into the parked
  run; or continue by handle with server-authoritative history.
- **Claim mechanism**: claim individual parked items; push decisions to a
  subscribed owner pod; or a compare-and-swap lease on the session, pulled by
  whichever pod receives the wake.
- **Storage home**: everything in the session store; everything in
  `memory_dir`; or a hybrid split by what must cross pods.
- **First durable backend**: Redis/Valkey first (#325's reference backend); or
  a file-backed store first.

## Decision Outcome

An orchestration run that hits an approval gate drains to a quiescent wave
boundary and commits one versioned checkpoint by compare-and-swap. A later
request holding the session handle claims the session under a fenced lease and
reifies the run, in the same process or a fresh one. The sections below are
the decision, numbered for reference; the domain types in
`crates/aura/src/orchestration/park/` are their reviewable form.

### 1. Quiescence rule

A worker hitting an approval gate durably parks that approval and its task
enters `Blocked { decision_id }`. The DAG scheduler keeps dispatching tasks
that are not transitively dependent on a blocked one. The run parks only when
the ready frontier is empty, nothing is running, and at least one task is
blocked: always a drained wave boundary. A future coordinator-level block
(#384) parks immediately; that surface is declared, not built.

Immediate whole-run park was rejected because it throws away work the DAG
could complete in parallel with the human. Mid-worker checkpointing was
rejected for V1 as decision 2.

### 2. No mid-worker resume in V1

A blocked task's worker attempt re-runs on reify with the decision available.
Pre-gate tool calls may re-execute; this is a documented limitation, bounded
by decision 9: the *approved* call itself is consumed exactly once, bound to
its decision id and arguments digest. Mid-worker conversation serialization is
out of scope until evidence shows re-execution cost matters.

### 3. Two-level FSM

The durable run FSM lives in the session store, schema-versioned:

```text
Created -> Running -> Parked { reason, parked_at, expires_at, checkpoint }
Parked  -> Running            (reify, on a durable wake reason)
Running -> Completed | Failed { cause } | Cancelled
Parked  -> Failed { cause }   (every non-human exit; fail-closed)
```

The `Parked` state carries its checkpoint (the resume point lives inside
it), so a parked record without a checkpoint is unrepresentable rather
than merely forbidden.

Terminals stay at three. **Expiry is a failure cause, not a fourth terminal**:
the reaper terminalizes an expired park as `Failed { cause: ParkExpired }`.
This keeps every consumer's terminal handling at three arms while retrieval
can still say why the run died. A run may durably exist `Parked` only at a
wave or iteration boundary.

The task FSM gains `Blocked { decision_id }`, with `Blocked -> Pending` on
decision. Fine-grained phases (Routing, Planning, ExecutingWave, Synthesizing,
Replanning) remain SSE observability only and are never persisted as state.

In the durable checkpoint, a blocked task is stored as `Pending` plus a
`BlockedTaskBinding { task, decision_id }` entry. The runtime `Blocked`
variant is scheduler-facing and is reconstructed on reify from the bindings.
This avoids a second persistent task-state vocabulary drifting from the
runtime one.

### 4. Continuation by handle, never transcript merge

A request referencing a parked run (session handle or approval id) gets
server-authoritative history; client messages are new trailing turns only. An
optional prefix hash lets a client assert the history it believes it is
continuing; a mismatch rejects the request. A `/v1/chat/completions` call
without the handle is a new conversation, never an implicit resume.

While the identity ADR (T1-D) is deferred, **handles are bearer
capabilities**: possession of the session handle or decision id is the only
authorization. This inherits the server's existing named auth gap and is
listed as a negative consequence.

### 5. Claim is a CAS lease on the session

Parked items are wake reasons (a HITL decision today; A2A messages, schedules,
and monitors arrive with their own work) that trigger a claim attempt. The pod
that receives the wake pulls and claims; there is no push subscriber. The
lease carries holder, heartbeat, expiry, and a monotonic fencing generation;
every session mutation presents its generation and is rejected as stale if a
newer owner exists. The initial attended run claims the session the same way,
so there is one ownership story, not an attended special case.

Per-item claims were rejected because the unit of consistency is the session:
one non-terminal run, one owner. Push delivery was rejected because it
requires a standing subscriber per parked run and re-introduces the
pod-affinity problem the store was meant to remove.

### 6. Storage split

The session store holds what must survive the pod: the run FSM record, lease,
parked-approval records, coordinator conversation (structured rig `Message`
values), `Plan` snapshot, original query, external chat history, budget and
timing state, config fingerprint, and approved-call bindings. Worker traces,
artifacts, and scratchpad contents stay on pod-local `memory_dir`.

All-in-store was rejected because artifacts and scratchpad are an
object-storage question (T2-F), not session state. All-in-`memory_dir` was
rejected because a file tree on one pod's disk cannot back a cross-pod claim
protocol, and #325's backend needs one contract to implement.

### 7. Atomic checkpoint

Park commits one versioned `RunCheckpoint` blob in a single CAS transition
`Running -> Parked`; the checkpoint is embedded in the `Parked` state, so
the transition and the blob are one value and one write. No scattered
writes; a reader sees the old state or the complete checkpoint. The checkpoint embeds the completed-task outputs future
DAG waves need. Pod-local references (artifact paths, scratchpad pointers)
are carried explicitly; reify on a pod that cannot resolve them refuses
rather than resuming with silent gaps.

### 8. Decisions are never destroyed

`ApprovalStore::resolve` today removes the parked entry at resolution; that
changes. Resolution persists a durable wake reason. The park commit reconciles
decisions that arrived during drain: if the block that caused the drain was
resolved before the CAS, the run continues instead of parking; otherwise the
wake reason is durable and survives until claimed.

### 9. Approval dispatch FSM

Consuming a *granted* decision is its own FSM, distinct from resolving the
approval (decision 8). A dispatch record exists only once an approval is
granted, so a denial has no state here and cannot reach execution by
construction: the fail-closed guarantee is the shape of the type, not a
guard.

```text
Unclaimed -> Claimed -> Executed | ExecutionUnknown
```

A crash after claim leaves `ExecutionUnknown`, never a silent retry, because
the gated call may have executed. The binding is `(decision_id, args_digest)`
where the digest is SHA-256 over the RFC 8785 (JCS) canonical form of the tool
arguments, recorded when the approval is parked. Dispatch presents the digest
of the arguments it is about to execute; a mismatch denies the call and leaves
the decision unconsumed: it never applied to those arguments. This is the
durable form of the #191 exactly-once invariant.

### 10. Request-scope teardown transfer

`RequestResourceGuard` deletes a request's approvals when its SSE stream ends.
Parking transfers approval ownership from request scope to session scope
before the response closes, so park-induced stream closure does not cancel the
approvals the park exists to preserve. A staged test proves this ordering.

### 11. Typed outcomes through the stack

`execute()` stringifies worker errors today, which would erase a park signal
into a `Failed` task. New intermediate outcome types
(`ToolAttemptOutcome::Blocked(ApprovalRef)`, `TaskExecutionOutcome`,
`WaveOutcome`, and a `Parked` arm on the iteration outcome) carry the block
as data from the tool layer to the run loop, so parking is a typed path, not
a parsed error string.

### 12. Expiry

Expiry reuses the HITL timeout configuration surface. A durable reaper
(scanner or durable timer, claiming under the same lease-and-fencing rules,
idempotent on terminalization) owns unattended expiry, because a live Tokio
timer dies with its process. Run-level expiry denies all outstanding approvals
and terminalizes the run `Failed { cause: ParkExpired }` with a summary. Task
budget clocks suspend while a task is blocked; a human's think time is not
task compute time.

### 13. Identity headers

Forwarded headers get a per-header TOML classification: `identity` (a
reified user id behind a trusted gateway; persisted in the checkpoint and
replayed on reify) or `credential` (the default). Credential-classified
headers are unparkable in V1: the gate refuses to park and names the header,
fail-closed, rather than persisting a secret or resuming without one.
`CredentialSource` is `StaticConfig | RequestForwarded | ServiceIdentity |
BrokeredDelegation`; the last two are declared for the identity ADR (T1-D)
and not constructed here. This matches the prevailing agent-platform pattern:
checkpoints store credential references, never secrets; unattended work runs
under service identity or brokered grants.

### 14. FileSessionStore

A file-backed session store backend ships as a real, supported backend, not
a test fixture. It preserves the zero-infrastructure standalone constraint
and lets the durability harness prove true process-restart park/reify. V1's
claim is a backend-independent park/reify protocol; cross-pod correctness
arrives with the Redis/Valkey backend (#325). Single-pod honesty: nothing in
V1 claims cross-pod behavior the file backend cannot deliver.

### 15. Retrieval is snapshot-authoritative

The `Parked` SSE frame emits only after the CAS succeeds, so a client that
saw the frame can always retrieve the run it names. Retrieval by handle
returns the current snapshot; event replay with cursors is future work.

### 16. Session identity

Today's `chat_session_id` is a client-supplied, reused string. It cannot be
the durable identity: a client-chosen value is guessable and collidable, and
decision 4 makes the session handle a bearer capability. So:

- A **session** is identified by a server-generated UUID (v7), minted at
  session creation. This is the reify handle.
- The client-supplied value becomes `ChatSessionId`, an external correlation
  key stored on the session record. It namespaces `memory_dir` runs and joins
  logs; it is never a capability and alone cannot reify a run.
- Invariant: **at most one non-terminal run per session**. The handle names
  the session; the session names its run.
- The existing `SessionId` newtype in `aura::config` wraps the client string
  and is renamed `ChatSessionId` when call sites are wired (P2 of #271), so
  the name `SessionId` means the durable identity everywhere.

### 17. Domain glossary

- **Session**: durable UUID identity owning the conversation, artifacts, and
  FSM record. Hydratable on any pod; executes nowhere.
- **AgentInstance**: a reified execution environment on one pod, born by
  claiming a session, holding an optional request id (attended) and task id.
  The TOML `[agent]` table keeps its meaning: it is the agent *definition*
  (model, tools, prompts); an `AgentInstance` is one live occupation of a
  session by a process. The two words collide in prose, so the type name
  carries the suffix.
- Artifacts and turns belong to sessions; tasks have a session and an agent
  instance.

Mike's `/aura` endpoint sketch (chat, list_session, load_session, kill, park,
schedule, list_approvals) shapes these types but is not built in V1.

### Positive Consequences <!-- optional -->

- An orchestration run is no longer bounded by its SSE stream or its host
  process. The attended prompt outlives the connection that carried it.
- Fail-closed becomes structural in the durable layer: three terminals, every
  non-human exit from `Parked` is `Failed`, digest-bound at-most-once
  consumption.
- The claim protocol is backend-independent; #325's Redis backend implements
  the same contract the file backend proves.
- Standalone deployments get durable parking with zero new infrastructure.

### Negative Consequences <!-- optional -->

- Pre-gate tool calls in a blocked worker may re-execute on reify (decision
  2). Bounded, documented, and revisited only with evidence.
- Session handles are bearer capabilities until the identity ADR lands
  (T1-D). Anyone holding a handle can reify its run.
- The checkpoint embeds the coordinator conversation, which is unbounded
  until compaction (#214) lands, a cost prerequisite rather than a correctness
  one.
  Large checkpoints make parking slow before they make it wrong.
- The blocked task has two representations: runtime `Blocked` variant,
  durable `Pending` + binding. The reify path must reconstruct one from the
  other, and a bug there surfaces as a task silently re-pending without its
  decision.
- Credential-classified headers make some runs unparkable by design. That is
  the correct V1 behavior, and it will surprise operators until T1-D provides
  service identity.
- A file backend proves durability, not concurrency: single-pod only until
  #325.

## Pros and Cons of the Options <!-- optional -->

### Drain to a quiescent wave boundary (chosen)

- Good: parallel work completes while a human decides; park points are the
  same boundaries the plan loop already reasons about.
- Good: the checkpoint never contains a mid-flight worker conversation.
- Bad: a blocked task's pre-gate work may re-execute on reify.

### Park immediately on first block

- Good: smallest time-to-park.
- Bad: discards the DAG's independent progress; rejected.

### Mid-worker checkpoint

- Good: no re-execution.
- Bad: serializes a live worker conversation mid-tool-loop, the hardest state
  in the system, for a cost V1 has no evidence justifies; rejected for V1.

### Transcript merge continuation

- Good: no server-side history required.
- Bad: the client becomes authoritative over what the human approved between
  park and reify, the same tampering surface the config gate exists to
  close; rejected.

### Push decisions to a subscribed owner

- Good: lowest wake latency.
- Bad: requires a standing subscriber per parked run and reintroduces pod
  affinity; rejected in favor of pull-and-claim.

### Redis-first backend

- Good: cross-pod from day one.
- Bad: makes durable parking depend on infrastructure the standalone
  deployment forbids, and couples this work to #325's schedule; rejected as
  the *first* backend.

## Links <!-- optional -->

- Blocks / is implemented by: [#271](https://github.com/mezmo/aura/issues/271)
- Builds on [2026-07-08-session-storage.md](2026-07-08-session-storage.md)
  (store/bus traits, live-handle rule) and
  [2026-06-16-hitl-approval-architecture.md](2026-06-16-hitl-approval-architecture.md)
  (fail-closed, dual-channel routing)
- Design note: [docs/design/hitl.md](../design/hitl.md) (V1 registry and route
  types this work extends)
- Cross-pod backend: [#325](https://github.com/mezmo/aura/issues/325); identity
  and auth: T1-D (unfiled); coordinator-mediated approvals:
  [#384](https://github.com/mezmo/aura/issues/384); coordinator compaction:
  [#214](https://github.com/mezmo/aura/issues/214)
- RFC 8785 (JSON Canonicalization Scheme):
  <https://www.rfc-editor.org/rfc/rfc8785>
- RFC 2119: <https://www.rfc-editor.org/rfc/rfc2119>
