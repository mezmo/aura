<!-- markdownlint-disable MD033 -->
# Pluggable session storage for cross-pod state

- Status: **accepted**
- Deciders: Justin Gross
- Date: 2026-07-08

Technical Story: [#325](https://github.com/mezmo/aura/issues/325) (Session storage)

## Context and Problem Statement

Aura keeps every piece of cross-request state in one process's memory. Behind a
load balancer with no session affinity — which is how the Helm production values
already run Aura (3–10 replicas, pods spread by anti-affinity, `Deployment` not
`StatefulSet`, no shared volume) — that state is invisible to the other pods. Two
subsystems break the moment a second replica exists:

- **HITL conversational approvals.** A tool call parks on one pod and streams an
  attended prompt down its SSE connection. The human's decision returns as a
  separate `POST /v1/approvals/{decision_id}` that the load balancer can route to
  any pod. A pod that never parked the call has no record of the `decision_id`,
  so the decision is lost and the call fails closed. The HITL ADR
  ([2026-06-16](2026-06-16-hitl-approval-architecture.md)) named this as its
  standing limitation and deferred the fix to this decision.
- **A2A task registry.** `message:send` creates a task in one pod's in-memory
  store and returns immediately. The client's follow-up `get` / `list` /
  `subscribe` / `cancel`, and conversation history rebuilt by `context_id`, all
  assume the same pod. The store even carries a `// TBD: a resilient location`
  marker in code.

The two subsystems are already shaped for the change: A2A's task store is behind a
trait, and HITL already splits a serializable parked-approval record from its
non-serializable in-process wake handle. What is missing is a decision about *how*
durable, shared state enters the system — and whether it becomes a hard dependency
for everyone or an opt-in.

Aura today has **no** external-storage dependency anywhere in the workspace, and a
meaningful share of its usage is single-process: CLI standalone mode, tests, and
local development. Any answer must not force infrastructure on those paths.

## Decision Drivers <!-- optional -->

- Cross-pod state **MUST NOT** become a hard runtime dependency for the default,
  single-process deployment. CLI standalone, tests, and local dev keep working
  with zero new infrastructure.
- The rest of the codebase **MUST** depend on an Aura-owned abstraction, not on a
  specific storage product. A team that runs Postgres, DynamoDB, etcd, or an
  in-house store **MUST** be able to satisfy the contract without changing callers.
- Selecting the default (in-process) backend **MUST** reproduce today's behavior
  exactly. This design is not allowed to change how a single pod behaves.
- The HITL fail-closed guarantee **MUST** survive: any outcome other than an
  explicit human approval still denies, including a storage or transport failure.
- Only serializable records **SHOULD** cross the pod boundary. Live runtime handles
  (wake channels, cancellation tokens, broadcast senders) stay pod-local and are
  re-established, not persisted.
- The design **SHOULD** cover only the state that is genuinely broken across pods.
  State that is correctly pod-local (event routing within one live SSE stream) is
  out of scope and **MUST NOT** be externalized.

## Considered Options

- Pluggable storage behind Aura-owned traits, in-process default, one shipped
  networked backend (this decision)
- A single hardcoded backend (bind directly to one storage product)
- Sticky session affinity, keep all state in-process
- A shared durable store with no event bus

## Decision Outcome

Chosen option: **pluggable storage behind Aura-owned traits.** Cross-pod state is
reached through a small set of Aura-defined storage traits. Aura ships one
networked reference implementation (Redis/Valkey-shaped, chosen because it
provides both capabilities below from one connection); the existing in-process
structures become the default implementation of the same traits; anyone may supply
their own backend by implementing the traits. Callers depend on the trait, never on
the backend.

Two capabilities are recognized as distinct traits, because cross-pod correctness
needs both and a shared store alone does not suffice:

- **A durable store** — survives restart, readable by any pod. It carries the
  serializable records: the parked approval and the A2A task. This alone makes
  every *read* path (poll a task, list by context, reconstruct history, validate a
  decision id) work on any pod.
- **An event bus (pub/sub)** — the mechanism that reaches a pod holding live state.
  A suspended approval's wake handle and a running A2A task's stream and cancel
  handle cannot be serialized and cannot move; they live on the pod that owns the
  request. The bus delivers a decision to the parking pod to wake it, fans a running
  task's updates out to subscribers on other pods, and routes a cancel to the pod
  running the task. Store plus bus together make approvals and the full A2A surface
  cross-pod with **no** session affinity required.

The default backend keeps both capabilities in-process (a local map and a local
broadcast), which collapses to exactly today's single-pod behavior. Selecting the
in-process backend is the regression guard: it must be byte-for-byte current
behavior.

Scope is confined to the state that is actually broken across pods: HITL
conversational approvals and the entire A2A task surface (including streaming and
cancel over the bus). Three things are deliberately excluded:

- **Request-scoped event routing** (the per-request tool and approval brokers, the
  cancellation registry) correlates events *within a single live SSE stream*, which
  is always anchored to one pod for its lifetime. It is correct as pod-local state
  and is not externalized; it only ever appears as a *target* of the bus, never as
  stored state.
- **On-disk scratchpad and orchestration artifacts** are a separate object-storage
  question (shared filesystem / blob store), not session state.
- **Server-side chat persistence** is unnecessary: OpenAI-compatible history is
  client-supplied, so that path is already stateless and horizontally scalable.

Implementation specifics — trait method sets, the backend's key/topic schema,
serialization format, configuration surface, and the issue/PR sequencing — live in
the design note, not in this decision.

### Positive Consequences <!-- optional -->

- Multi-pod deployment becomes correct, closing the single-pod limitation the HITL
  ADR called out and the A2A `TBD` marker anticipated.
- Cross-pod state is opt-in. With no session store configured, behavior is
  unchanged and no new infrastructure is required — CLI standalone, tests, and
  local dev are untouched.
- The trait boundary keeps the shipped backend replaceable. Operators are not
  locked to one storage product.
- Fail-closed remains structural: a storage or bus failure denies the approval
  rather than admitting an unapproved call.
- Only plain data crosses the boundary, so the hard-to-serialize runtime handles
  never have to be, and the blast radius stays inside two subsystems.

### Negative Consequences <!-- optional -->

- The shipped multi-pod story adds an operational dependency (a networked store)
  to deploy, secure, and run — even though the default build does not require it.
- Two capabilities (store and bus) are more surface to define and test than a
  single store would have been.
- Best-effort pub/sub can drop a wake or a streamed frame across a transient
  disconnect. The store's expiry and the parking pod's timeout keep this
  fail-closed, but a missed wake can waste an approval's timeout window; a
  delivery-guaranteed transport is a heavier alternative left to implementation.
- Neither the decision ingress nor the backend connection carries an auth layer of
  its own; that inherits the server's existing named auth gap and the deployment's
  network controls.

## Pros and Cons of the Options <!-- optional -->

### Pluggable storage behind Aura-owned traits

- Good, callers depend on an Aura contract, so the backend is swappable and the
  default stays in-process with no new infrastructure.
- Good, the in-process implementation is a byte-for-byte regression guard for
  single-pod behavior.
- Good, recognizing store and bus as separate capabilities is what actually makes
  approvals and A2A cross-pod, not just poll-able.
- Bad, two traits and a networked reference backend to build, document, and test.

### A single hardcoded backend

- Good, less abstraction; one path to implement.
- Bad, forces a storage dependency on every deployment including CLI standalone,
  tests, and local dev, and locks operators to one product. Rejected against the
  no-hard-dependency and own-abstraction drivers.

### Sticky session affinity, keep all state in-process

- Good, no code change; a load-balancer setting pins a client to its pod.
- Bad, does not survive a pod restart or eviction, fights autoscaling and
  pod anti-affinity, and cannot serve A2A's return-immediately-then-poll pattern
  where a poll legitimately arrives on another pod. A crutch, not a fix.

### A shared durable store with no event bus

- Good, simplest durable option; makes every read/poll path cross-pod.
- Bad, cannot wake a suspended approval or fan out a live stream, because those
  depend on pod-local handles a store cannot reach. Solves half the problem and
  leaves the attended and streaming paths broken.

## Links <!-- optional -->

- Design and implementation note: [docs/design/session-storage.md](../design/session-storage.md)
  (trait sketches, backend key/topic schema, configuration, phased rollout, testing)
- Fulfills [#209](https://github.com/mezmo/aura/issues/209) (durable cross-request
  state), the deferred successor named throughout the HITL ADR
- Unblocks the single-pod limitation in
  [2026-06-16-hitl-approval-architecture.md](2026-06-16-hitl-approval-architecture.md)
  (durable park-and-resume across processes)
- Relates to [docs.mezmo.com/aura/hitl](https://docs.mezmo.com/aura/hitl) and
  [docs.mezmo.com/aura/a2a-implementation](https://docs.mezmo.com/aura/a2a-implementation) (the two affected surfaces)
- RFC 2119: <https://www.rfc-editor.org/rfc/rfc2119>
</content>
