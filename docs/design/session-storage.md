<!-- markdownlint-disable MD033 -->
# Session storage (cross-pod state): design and implementation note

Companion to the ADR [2026-07-08-session-storage](../adr/2026-07-08-session-storage.md).
The ADR records the decision (cross-pod state behind Aura-owned traits, in-process
default, one shipped networked backend, store plus event bus as distinct
capabilities). This note carries the trait method sets, the backend key/topic
schema, the config and wire formats, the wiring changes, and the phased rollout.

**Status:** living design note, current as of 2026-07-13. The ADR holds the
durable decision. **Phase 1 of §11 (traits + in-memory refactor) is implemented:**
`aura::session_store` defines `ApprovalStore` and `EventBus` with in-memory impls,
`PendingApprovals` rides on them (wake handles stay local, decisions travel over the
bus), and the web server's `SessionStore` factory
(`crates/aura-web-server/src/session_store.rs`) composes them with the upstream A2A
`TaskStore`. The factory lives in the web-server crate rather than `aura` because
`a2a_server::TaskStore` is a web-server-only dependency; the capability traits stay
in `aura` so CLI standalone needs no A2A dependency. The Redis/Valkey backend
(phases 2–4), the `[session_store]` config surface (§8), `ParkedApproval` serde
derives, and Helm wiring (§10) are not built yet. References to existing aura code
(file:line, module paths) are where-to-look pointers and move as the code does.

**Goal of iteration 1:** make the state AURA _already has_ work across pods, so a
load-balanced multi-replica deployment (Helm prod runs 3–10 replicas, no session
affinity) behaves the same as a single process. Do it behind **traits** so the
shipped backend (Redis/Valkey) is one implementation, not the only one.

## TL;DR

- AURA today keeps every piece of cross-request state **in one pod's RAM**. Behind a
  load balancer with no session affinity, that state is invisible to the other pods.
- Two subsystems break in a multi-pod deployment **today**: **HITL conversational
  approvals** and the **A2A task registry**. Both are explicitly flagged in code as
  "not durable / TBD resilient location".
- The fix is a small set of **AURA-owned storage traits**. We ship a **Redis/Valkey**
  implementation; the existing in-memory behavior stays as the default `impl`; anyone
  can write their own backend against the traits.
- A shared store alone is not sufficient. Two mechanisms are needed together:
  1. a **durable store** (survives restart, readable by any pod), and
  2. an **event bus** (pub/sub) to **wake** the pod that owns a suspended request and
     to fan out streaming updates across pods.
     Redis/Valkey gives us both from one connection.

---

## 1. Why this is needed — the cross-pod problem

The web server is effectively **stateless per HTTP request** for conversation content:
OpenAI-compatible clients resend the full `messages[]` array every call, so chat
history lives on the client. That part already scales horizontally.

The problem is the handful of things that **must survive across two separate requests**
or **across a request and a later poll**, all of which live in per-pod memory today:

| State                                           | Where it lives today                                           | Crosses requests? | Survives restart? | Shared across pods? |
| ----------------------------------------------- | -------------------------------------------------------------- | ----------------- | ----------------- | ------------------- |
| CLI conversations                               | `~/.aura/conversations/<uuid>/` (client disk)                  | —                 | yes               | n/a (client-side)   |
| Web chat history                                | client-supplied `messages[]`                                   | —                 | n/a               | n/a (stateless)     |
| `chat_session_id`                               | correlation string (tracing + `aura.session_info`)             | no state          | —                 | just an ID          |
| `request_cancellation` registry                 | `OnceLock<HashMap<RequestId, CancellationToken>>`              | request-scoped    | no                | no                  |
| `tool_event_broker` / `approval_event_broker`   | `OnceLock<HashMap<request_id, Sender>>`                        | request-scoped    | no                | no                  |
| **HITL `PendingApprovals`**                     | `Arc<Mutex<BTreeMap<DecisionId, PendingEntry>>>` on `AppState` | **yes**           | no                | **no** ← gap        |
| **A2A `SharedTaskStore` + `task_cancel_state`** | `Arc<InMemoryTaskStore>` / `Arc<Mutex<HashMap>>`               | **yes**           | no                | **no** ← gap        |
| Scratchpad / orchestration artifacts            | `{memory_dir}/...` (`tokio::fs`, often `/tmp`, no PVC)         | pod-local         | no                | no                  |

### Concrete failure modes today

- **HITL:** request R1 hits pod A, the agent parks a conversational approval, and the
  attended `aura.approval_pending` SSE event goes out on pod A. The human's
  `POST /v1/approvals/{decision_id}` is load-balanced to **pod B**, which has no entry
  for that `decision_id` → `ResolveError::NotFound` (404). The tool call on pod A then
  times out and fails closed. Approvals are simply unusable multi-pod.
  (`crates/aura/src/hitl/registry.rs` — the module doc already says _"a decision must
  land on the process that parked the call"_.)
- **A2A:** `POST /a2a/v1/message:send` on pod A creates a task in pod A's
  `InMemoryTaskStore` and returns immediately in `Working` state. The client's follow-up
  `GET /a2a/v1/tasks/{id}` is routed to **pod B** → task-not-found. Conversation
  continuity also breaks: history is rebuilt by listing prior tasks in the same
  `context_id` from the in-memory store, so a context that lands on another pod loses all
  prior turns. (`crates/aura-web-server/src/a2a/shared_task_store.rs:287` literally reads
  `// forcing an in-memory store for now. TBD: a resilient location`.)

Both subsystems are already architected for this change: A2A's `TaskStore` is an upstream
**trait** (the in-memory store is just one impl), and HITL deliberately splits a
serializable `ParkedApproval` from the non-serializable `oneshot::Sender` wake handle,
with a code comment pointing at "durable parking".

---

## 2. Design principles

1. **Trait-first, backend-second.** The rest of the codebase depends on a trait, never on
   Redis. The shipped Redis/Valkey backend is one `impl`. A team that wants Postgres,
   DynamoDB, etcd, or an in-house store implements the same traits and wires it in.
2. **In-memory stays the default.** With no session store configured, AURA behaves
   exactly as it does today — single pod, CLI standalone, tests, and local dev keep
   working with zero new infra. The in-memory `impl` _is_ today's code, moved behind the
   trait.
3. **Two capabilities, one backend.** Cross-pod correctness needs a durable **store** and
   a pub/sub **event bus**. Keep them as separate traits (a store-only backend is still
   useful for polling paths), but let one backend satisfy both — Redis/Valkey does.
4. **Serialize the record, never the runtime handle.** Only plain data crosses the pod
   boundary (`ParkedApproval`, A2A `Task`). Live handles (`oneshot::Sender`, tokio
   `broadcast`, `CancellationToken`) stay pod-local and are re-established via the bus.
5. **Fail closed, degrade to today.** If the store/bus is unavailable, HITL still fails
   closed (deny), and a single-pod deployment still works. No new hard dependency for the
   default path.
6. **Small blast radius.** Iteration 1 touches only the two subsystems that are broken
   multi-pod. Request-scoped brokers and on-disk artifacts are explicitly out of scope
   (see §3).

---

## 3. Scope

### In scope (iteration 1)

- **HITL conversational approvals** — durable parked-approval store + cross-pod wake so a
  decision can be resolved on any pod.
- **A2A task registry** — durable, shared `TaskStore` so create / get / list / poll work
  on any pod, and conversation history by `context_id` survives load-balancing.
- **A2A streaming + cancel over the bus** — `message:stream`, `tasks/{id}:subscribe`, and
  `cancel` work cross-pod: task updates fan out over the event bus and `cancel` is routed
  to the pod running the task. No session affinity required for any A2A flow.

### Explicitly out of scope (later iterations)

- **Request-scoped brokers** (`tool_event_broker`, `approval_event_broker`,
  `request_cancellation`). These correlate events _within a single live SSE stream_,
  which is always anchored to one pod for its lifetime. They are correct as pod-local
  state and must **not** be externalized. They only enter the picture as the _bus_
  targets for cross-pod wake/fan-out (§6), not as stored state.
- **Scratchpad / orchestration artifacts** under `memory_dir`. Today these are pod-local
  disk (often `/tmp`, no PVC). Making them durable/shared is a separate object-storage
  discussion (S3/GCS/PVC), not session state. Noted here so it is not forgotten.
- **Server-side chat persistence** for `/v1/chat/completions`. History is client-supplied;
  we are not adding a server-side conversation store in this iteration.

---

## 4. The core abstraction

Two capabilities. A backend may provide one or both; Redis/Valkey provides both.

```text
    ┌─────────────────────────── SessionStore (factory) ───────────────────────────┐
    │                                                                              │
    │   approvals() ─────▶ ApprovalStore   (durable parked HITL approvals)         │
    │   tasks()     ─────▶ TaskStore       (durable A2A tasks — upstream trait)    │
    │   bus()       ─────▶ EventBus        (pub/sub: wake + streaming fan-out)     │
    │                                                                              │
    └──────────────────────────────────────────────────────────────────────────────┘
              │                                   │
  in-memory (default, today's behavior)     redis/valkey (shipped)
```

`SessionStore` is a thin factory so `main` constructs one backend and hands out the
capability handles. Everything downstream depends on the capability traits, not on
`SessionStore` or the concrete backend.

```rust
/// A pluggable backend for cross-pod session state. Constructed once in `main`
/// from config; hands out capability handles. One backend, multiple capabilities.
#[async_trait]
pub trait SessionStore: Send + Sync {
    fn approvals(&self) -> Arc<dyn ApprovalStore>;
    fn tasks(&self) -> Arc<dyn TaskStore>;      // A2A: upstream `a2a_server::TaskStore`
    fn bus(&self) -> Arc<dyn EventBus>;

    /// Cheap liveness check surfaced on `/health` and at startup.
    async fn ping(&self) -> Result<(), SessionStoreError>;
}
```

---

## 5. The traits

### 5.1 `ApprovalStore` — durable parked approvals

Captures exactly the operations `PendingApprovals` performs today, but over the
**serializable** `ParkedApproval` record instead of an in-RAM `BTreeMap`. The
non-serializable `oneshot::Sender` wake handle stays pod-local (see §6.1).

```rust
#[async_trait]
pub trait ApprovalStore: Send + Sync {
    /// Persist a parked approval. Keyed by `DecisionId` (UUID v7, time-ordered).
    /// TTL should track `expires_at` so abandoned approvals self-clean.
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError>;

    /// Look up a parked approval (used by the resolving pod to validate the id
    /// exists and is not expired before publishing the decision).
    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError>;

    /// Record a terminal decision and remove the parked entry, atomically.
    /// Returns `NotFound` if unknown / already resolved / expired — preserving
    /// today's at-most-once semantics.
    async fn resolve(&self, id: &DecisionId, decision: ApprovalDecision)
        -> Result<(), ResolveError>;

    /// Remove a parked entry (timeout / cancellation).
    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError>;

    /// Remove every approval parked under a request id (stream drop / shutdown).
    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError>;
}
```

`ParkedApproval` is already serialization-ready in `crates/aura/src/hitl/registry.rs`
(`request: ApprovalRequest`, `registered_at`, `expires_at`). It only needs
`#[derive(Serialize, Deserialize)]` added (and the same on `ApprovalRequest` /
`ApprovalDecision`, which are wire-shaped already).

> The **webhook** HITL route needs none of this — it is a synchronous outbound POST that
> holds no cross-request state, so it is already cross-pod safe. `ApprovalStore` is only
> for the **conversational** route.

### 5.2 `TaskStore` — A2A tasks (reuse the upstream trait)

A2A already defines the seam. `a2a_server::TaskStore` is an `async_trait` with
`create` / `update` / `get` / `list` returning `TaskVersion`. Today's
`SharedTaskStore(Arc<InMemoryTaskStore>)` is one impl; we add a Redis/Valkey impl of the
**same** trait. AURA's `merge_artifacts()` fix stays as a wrapper regardless of backend.

```rust
// Existing wrapper, backend swapped underneath:
pub struct SharedTaskStore(Arc<dyn a2a_server::TaskStore>);   // was Arc<InMemoryTaskStore>
```

`Task` is already `serde`-serializable, so a Redis impl stores each task as a JSON (or
MessagePack) value under `a2a:task:{id}`, plus a `context_id → [task_id]` index for
`list` and history reconstruction.

### 5.3 `EventBus` — cross-pod wake + streaming fan-out

The piece a shared store alone cannot provide. Publishing a small message to a topic and
subscribing to a topic from another pod. Redis/Valkey pub/sub (or Streams) backs it; the
in-memory impl is a local `tokio::broadcast` registry (single-pod, identical to today's
behavior).

```rust
#[async_trait]
pub trait EventBus: Send + Sync {
    /// Publish an opaque payload to a topic. Fire-and-forget; delivery is best-effort.
    async fn publish(&self, topic: &str, payload: Bytes) -> Result<(), SessionStoreError>;

    /// Subscribe to a topic. The stream ends when the subscription is dropped.
    async fn subscribe(&self, topic: &str)
        -> Result<Pin<Box<dyn Stream<Item = Bytes> + Send>>, SessionStoreError>;
}
```

Topics (iteration 1):

- `approval:{decision_id}` — the resolving pod publishes the `ApprovalDecision`; the
  parking pod is subscribed and fires its local `oneshot` to wake the suspended tool call.
- `a2a:task:{task_id}` — the pod running a task publishes status/artifact updates; any pod
  serving `message:stream` / `subscribe` relays them to its client.
- `a2a:cancel:{task_id}` — a `cancel` on any pod publishes here; the pod running the task
  is subscribed and fires its local `CancellationToken`.

---

## 6. How each subsystem changes

### 6.1 HITL conversational approvals (cross-pod)

The key realization: the **attended SSE stream stays on the parking pod** (that is where
the human is watching `aura.approval_pending`), but the **decision can arrive on any
pod**. So we do _not_ need to move the stream — we need the decision to reach the parking
pod. Store + bus does exactly that.

```text
 Pod A (parks)                          Redis/Valkey                    Pod B (resolves)
 ───────────────                        ─────────────                   ────────────────
 tool gated, decide()
   │
   ├─ approvals.register(parked) ─────▶ SET approval:{id} EX ttl
   ├─ bus.subscribe("approval:{id}")──▶ SUBSCRIBE approval:{id}
   ├─ emit aura.approval_pending (SSE, local stream)
   └─ await oneshot / timeout / cancel
                                                                 POST /v1/approvals/{id}
                                                                        │
                                        resolve approval:{id} ◀─────────┤ approvals.resolve(id, dec)
                                        PUBLISH approval:{id} ◀─────────┤ bus.publish("approval:{id}", dec)
   bus stream yields decision  ◀──────────────────┘
   fire local oneshot ─▶ await returns ─▶ tool proceeds / blocked
```

Changes:

- `PendingApprovals` keeps its in-RAM map of **`decision_id → oneshot::Sender`** (the wake
  handles — inherently pod-local). It gains an `Arc<dyn ApprovalStore>` and an
  `Arc<dyn EventBus>`.
- `register()` now (a) inserts the local `oneshot` as today **and** (b)
  `approvals.register(parked)` + `bus.subscribe("approval:{id}")`, with a small task that
  fires the local `oneshot` when the bus yields a decision.
- The `POST /v1/approvals/{id}` handler calls `approvals.resolve()` then
  `bus.publish("approval:{id}", decision)`. It no longer needs the parked entry to be
  local — it may run on any pod.
- `cancel_request()` deletes matching store entries and (optionally) publishes a cancel so
  the parking pod stops waiting.
- **Default backend = in-memory** collapses this back to exactly today's behavior:
  `resolve()` finds the local entry and fires the `oneshot` directly; `subscribe`/`publish`
  are a local broadcast. Single-pod and CLI standalone are unchanged.

Timeout/expiry stays authoritative on the parking pod's `await` (as today), with the store
TTL as a backstop so an abandoned parking pod's entry self-cleans.

### 6.2 A2A task registry (cross-pod)

- **Biggest win, smallest change:** swap `SharedTaskStore`'s inner `Arc<InMemoryTaskStore>`
  for `Arc<dyn TaskStore>` and construct the Redis/Valkey impl in `main`. This alone makes
  `message:send` → `GET /tasks/{id}` → `list` → history-by-`context_id` all work across
  pods, because those are **store reads**, not live streams. Given A2A's `message:send`
  returns immediately and the documented pattern is _poll `GET /tasks/{id}`_, this covers
  the primary flow.
- **Streaming (`message:stream`, `tasks/{id}:subscribe`) and `cancel`** use the bus,
  because the execution runs on the pod that received `message:send` and its live SSE
  fan-out channel and cancel handle are pod-local. Two topics bridge them:
  - `a2a:task:{id}` — the running pod publishes every task status/artifact update; a
    `subscribe` handler on **any** pod subscribes to it and relays frames to its own
    client. This is one-producer-to-many-subscribers **fan-out** across pods.
  - `a2a:cancel:{id}` — a `cancel` request landing on any pod publishes here; the owning
    pod is subscribed and fires its local `CancellationToken`.
  With the shared `TaskStore` and these two topics, **every** A2A flow — send, poll, list,
  history, stream, subscribe, cancel — is cross-pod. No session affinity is needed.

---

## 7. Reference implementation — Redis / Valkey

Valkey is the default target (BSD-licensed Redis fork; Redis wire-compatible, so one
client library — `redis-rs` or `fred` — talks to either). One connection pool provides
both the store and the bus.

### Key schema (iteration 1)

| Key / channel                    | Type                           | Purpose                                | TTL                      |
| -------------------------------- | ------------------------------ | -------------------------------------- | ------------------------ |
| `aura:approval:{decision_id}`    | string (JSON `ParkedApproval`) | parked approval record                 | `expires_at`             |
| `aura:approval:req:{request_id}` | set of `decision_id`           | `cancel_request` fan-out               | slightly > route timeout |
| `approval:{decision_id}`         | pub/sub channel                | wake the parking pod with the decision | —                        |
| `aura:a2a:task:{task_id}`        | string (JSON `Task`)           | A2A task record                        | configurable (e.g. 24h)  |
| `aura:a2a:ctx:{context_id}`      | list/zset of `task_id`         | history + `list` by context            | same as task             |
| `a2a:task:{task_id}`             | pub/sub channel                | streaming fan-out to subscribers       | —                        |
| `a2a:cancel:{task_id}`           | pub/sub channel                | route `cancel` to the pod running it   | —                        |

Notes:

- `resolve` is a Lua script / `MULTI` so "read-and-delete + record decision" is atomic and
  at-most-once, matching today's `remove`-on-resolve.
- All keys are namespaced under `aura:` and should carry a deployment/tenant prefix
  (config, §8) so multiple AURA deployments can share a cluster.
- Use `SET ... EX` for TTL; no background sweeper needed. The parking pod's `await` remains
  the authoritative timeout.

### In-memory default impl

The existing structures become the in-memory backend, unchanged in behavior:

- `ApprovalStore` (in-memory) = today's `Arc<Mutex<BTreeMap<DecisionId, ..>>>`.
- `TaskStore` (in-memory) = the upstream `InMemoryTaskStore`.
- `EventBus` (in-memory) = a local `HashMap<topic, tokio::broadcast::Sender>` registry.

Selecting the in-memory backend must produce byte-for-byte today's behavior — this is the
regression guard for the refactor.

---

## 8. Configuration surface

Default is in-memory (no new infra). Enable a shared backend explicitly.

```toml
# Optional. Absent → in-memory (single-pod behavior, today's default).
[session_store]
backend = "redis"                     # "memory" (default) | "redis"
url = "{{ env.AURA_SESSION_STORE_URL }}"   # redis:// or rediss:// (Valkey compatible)
key_prefix = "aura:prod"              # namespace; lets deployments share a cluster
connect_timeout_secs = 5
# TTLs
approval_ttl_secs = 0                 # 0 → derive from each approval's expires_at
task_ttl_secs = 86400
```

Env-var equivalents for the 12-factor / Helm path (mirrors existing AURA env conventions):

| Env var                     | Meaning                                |
| --------------------------- | -------------------------------------- |
| `AURA_SESSION_STORE`        | `memory` (default) or `redis`          |
| `AURA_SESSION_STORE_URL`    | `redis://…` / `rediss://…` (Valkey ok) |
| `AURA_SESSION_STORE_PREFIX` | key namespace                          |

Cargo feature flags keep the default build free of the Redis client:

```toml
# aura-web-server / aura
[features]
default = []
session-store-redis = ["dep:fred"]    # or redis-rs
```

Build with `--features session-store-redis` for the shipped image; the trait and the
in-memory impl are always compiled.

---

## 9. Wiring changes

- **`main.rs`**: construct one `Arc<dyn SessionStore>` from config (memory or redis),
  `ping()` it at startup (fail fast if `backend = redis` and unreachable), then:
  - `pending_approvals` is built from `store.approvals()` + `store.bus()` instead of
    `PendingApprovals::new()` (`crates/aura-web-server/src/main.rs:257`).
  - `SharedTaskStore::new()` becomes `SharedTaskStore::from(store.tasks())`
    (`crates/aura-web-server/src/main.rs:287`).
- **`AppState`** (`crates/aura-web-server/src/types.rs:77`): `pending_approvals` keeps its
  type (now trait-backed inside); optionally add `session_store: Arc<dyn SessionStore>` for
  `/health` reporting.
- **CLI standalone** (`crates/aura-cli/src/backend/direct.rs:104`): keeps constructing the
  in-memory backend — no server, no Redis, unchanged.
- **`/health`**: include a `session_store` block (`backend`, `ping` ok/latency) alongside
  the existing `a2a_server` block.

The `RigBuilder` → `HitlRuntime` threading is unchanged in shape; only the concrete type
behind `PendingApprovals` gains the store/bus handles.

---

## 10. Deployment changes (Helm)

- Add an optional Valkey dependency (subchart or a pre-existing managed instance) and
  `values` for `sessionStore.enabled`, `url` (via Secret), `keyPrefix`, TTLs.
- Set `AURA_SESSION_STORE=redis` + `AURA_SESSION_STORE_URL` (from Secret) in the
  Deployment env when enabled.
- With the shared store plus the bus (approvals, A2A tasks, and A2A streaming/cancel all
  cross-pod), **no session affinity is required** — leave the Service at the default
  `sessionAffinity: None`. `ClientIP` or ingress cookie affinity is not needed and should
  not be added.
- `AURA_SERVER_URL` already must be the external origin behind the LB (documented in
  `docs/a2a-implementation.md`); no change, just reconfirm for multi-pod.

---

## 11. Phased rollout

1. **Traits + in-memory refactor (no behavior change).** ✅ **Implemented
   (2026-07-13).** Introduce `SessionStore`, `ApprovalStore`, `EventBus`; move today's
   structures behind them; A2A already has `TaskStore`. Ship with `backend = memory`.
   This is a pure refactor — existing tests are the guard.
2. **Redis/Valkey backend — A2A task store.** Smallest, highest-value cross-pod win
   (`message:send` + poll works across pods). No bus needed for the poll path.
3. **Redis/Valkey backend — HITL approvals (store + wake bus).** Cross-pod conversational
   approvals via `approval:{id}` pub/sub. Fail-closed preserved.
4. **A2A streaming/cancel over the bus.** Cross-pod `message:stream` / `subscribe` /
   `cancel` via the `a2a:task:{id}` (fan-out) and `a2a:cancel:{id}` (routed cancel) topics.
   Depends on the shared task store (phase 2) and the `EventBus` impl (phase 3).
5. **Helm/Valkey packaging + docs.** Values, Secret wiring, `/health`, runbook.

Each phase is a self-contained issue/PR. Phase 1 is the prerequisite for everything else;
phases 2 and 3 are independent and can land in either order; phase 4 needs both.

---

## 12. Testing

- **Trait conformance suite:** one async test battery run against _both_ the in-memory and
  Redis backends (Redis via `testcontainers` / an ephemeral Valkey) — register/resolve,
  at-most-once resolve, expiry, `cancel_request`, task create/get/list/history-by-context.
- **Cross-pod simulation:** two `AppState` instances sharing one backend; park an approval
  via instance A, resolve via instance B, assert instance A's await wakes. Same for A2A:
  `message:send` on A, `GET /tasks/{id}` on B.
- **Degradation:** `backend = redis` with the store down → startup fails fast; store lost
  mid-flight → HITL fails closed, A2A surfaces a clean error, single-pod in-memory path
  unaffected.
- **Regression:** the in-memory backend must pass the existing HITL (`registry.rs`,
  `route.rs`) and A2A (`a2a_test.rs`) suites unchanged.

---

## 13. Open questions

- **Bus reliability for wake.** Redis pub/sub is fire-and-forget; if the parking pod
  briefly disconnects it can miss the decision. The store TTL + await timeout make this
  fail-closed (safe) but a missed wake wastes the timeout window. Redis **Streams**
  (consumer read-after-write) would make the wake reliable at some complexity cost —
  decide per-subsystem.
- **At-most-once resolve across pods** must be enforced in the store (Lua/`MULTI`), not in
  application code, since two `POST /v1/approvals/{id}` could race on different pods.
- **Register faults: fail fast vs park-and-time-out.** `PendingApprovals::register` is
  infallible: store/bus faults are warn-logged and the call parks anyway, failing closed at
  its timeout. Safe, but when `subscribe` fails the park is _provably_ unwakeable at
  registration time, and the user still sees a `Pending` event and waits out the full
  timeout. With a networked backend (phase 3), consider short-circuiting to an immediate
  terminal outcome (or failing the gate) instead of emitting `Pending` for a dead park.
- **Resolve atomicity across store + bus.** `PendingApprovals::resolve` records the
  decision in the store, then publishes the wake. In-memory, both futures complete on
  their first poll, so the pair runs without a yield point and cannot be interrupted.
  With a networked backend, the resolving request can be dropped between the two calls
  (client disconnect mid-await) — decision consumed, wake never published, parked side
  fails closed at its timeout. Phase 3: shield the store+publish pair in a spawned task,
  or accept and document the window.
- **Teardown latency behind store I/O.** The request `Drop` guard cancels wake handles
  synchronously (`cancel_request_local`), but the spawned cleanup task awaits
  `ApprovalStore::cancel_request` before `RequestCancellation::unregister` and the
  event-broker unsubscribes run. A slow networked store stretches that tail cleanup —
  bound store calls with client-side timeouts when picking the phase-3 driver config.
- **Trace context across the bus.** Bus payloads carry no OTel trace context, so on a
  cross-pod resolve the parking pod's wake cannot link to the resolving request's trace
  (in-process, the wake task is instrumented with the registering request's span). Phase 3
  should decide whether decision payloads carry W3C `traceparent` for span links, or the
  two traces stay correlated only by `decision_id` attributes.
- **Surfacing swallowed backend faults.** The warn-only paths (`register` store/bus faults,
  `resolve` publish failure, `Drop`-guard cleanup failures) are unreachable with the
  in-memory backend but become real operational signals in phase 3 — add metrics/counters
  so operators detect a degraded backend without log-diving. Document the approvals API
  contract alongside: `204` means the decision was persisted and consumed at-most-once;
  delivery to the parked waiter is best-effort, backstopped by the fail-closed timeout.
- **Serialization format** for `ParkedApproval` / `Task` — JSON (debuggable) vs MessagePack
  (compact). Start with JSON.
- **Multi-tenant / multi-deployment** sharing one cluster — the `key_prefix` covers naming;
  auth/ACL is deployment infra, out of scope here.
- **A2A push notifications** are currently disabled (no push-config store wired). A shared
  push-config store is a natural extension of this same trait set, later.

---

## 14. Touch points (for implementers)

| Concern                                           | File                                                  |
| ------------------------------------------------- | ----------------------------------------------------- |
| HITL registry (→ `ApprovalStore` in-memory impl)  | `crates/aura/src/hitl/registry.rs`                    |
| HITL conversational route (register/await)        | `crates/aura/src/hitl/route.rs`                       |
| HITL decision ingress (`POST /v1/approvals/{id}`) | `crates/aura-web-server/src/handlers.rs:989`          |
| A2A store wrapper (→ `TaskStore` swap)            | `crates/aura-web-server/src/a2a/shared_task_store.rs` |
| A2A executor (streaming fan-out / cancel)         | `crates/aura-web-server/src/a2a/agent_executor.rs`    |
| Server wiring (construct backend)                 | `crates/aura-web-server/src/main.rs:257,287`          |
| `AppState`                                        | `crates/aura-web-server/src/types.rs:77`              |
| CLI standalone wiring (stays in-memory)           | `crates/aura-cli/src/backend/direct.rs:104`           |
| Config (`[session_store]`)                        | `crates/aura-config/src/config.rs`                    |
| Helm values / Deployment env                      | `deployment/helm/aura/`                               |

## 15. Related docs

- [../adr/2026-07-08-session-storage.md](../adr/2026-07-08-session-storage.md) — the ADR
  this note implements (the decision to put cross-pod state behind Aura-owned traits with an
  in-process default).
- [../hitl.md](../hitl.md) — HITL behavior; §"Current limitations" already names
  _"Conversational approvals are in-process only … Durable approval parking and cross-pod
  resume are not implemented yet."_ This doc is the plan to close that.
- [../a2a-implementation.md](../a2a-implementation.md) — A2A endpoints; _"Tasks are stored
  in the in-memory `TaskStore` for the lifetime of the process."_
- [../request-lifecycle.md](../request-lifecycle.md) — request/stream lifecycle,
  cancellation, shutdown (why the request-scoped brokers are correctly pod-local).
