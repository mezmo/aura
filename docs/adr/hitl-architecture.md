# ADR: Human-in-the-Loop (HITL) Approval Architecture

**Status:** Proposed
**Date:** 2026-06-10
**Authors:** Mike Shearer
**Related:** [issue #191](https://github.com/mezmo/aura/issues/191), [issue #174](https://github.com/mezmo/aura/issues/174), [issue #209](https://github.com/mezmo/aura/issues/209) (the State ADR)
**Code refs:** file:line references resolve against `mshearer/hitl-v1-config-gate` @ `52f37e6`
(pushed to origin), the prototype branch this ADR reworks — not against the commit this
document lands on.

## Context

Aura's agents execute tool calls unconditionally. There is no way to require human
sign-off before a destructive call runs, which blocks SRE dogfooding of the
orchestrated agent (#191): incident remediation needs an operator's approval before
anything irreversible happens.

Two deployment shapes need approval, and they differ structurally:

- **Unattended** (A2A, background): no human is on the stream; an external service
  answers approval requests.
- **Attended** (CLI, dev, dogfooding): the operator chatting with the agent over SSE
  is the approver. SSE is server-to-client only, so the approval prompt can ride the
  stream down, but the decision needs a path back up — and the server exposes no
  endpoint that accepts one.

A spike of the webhook half lives on `mshearer/hitl-v1-config-gate` (unmerged). It
validated the gate insertion point, both approval surfaces, and the fail-closed
policy; this design reworks it (see the keep / rework / discard map below).

## Decision: two decision routes, chosen by config

Approval requests travel one of two routes. The route is fixed per deployment in
TOML. There is no runtime attendance detection: the deployment knows whether it is
attended (CLI, dev, dogfooding) or unattended (A2A, background), so routing is pure
configuration.

### Route A: webhook (unattended)

One synchronous HTTP round-trip. The decision comes back in the response body.

```
Client ──POST /v1/chat/completions──► aura-web-server
                                         │ agent loop hits gate
                                         ▼
                                      hitl gate ──POST approval req──► webhook svc
                                         │ ◄──── {"approved": true} ──── (responds
                                         │   one HTTP round-trip,         inline)
                                         ▼   blocks ≤ timeout_secs
                                      tool executes / denied
Client ◄── SSE: approval_requested ... approval_completed ... tool result
```

### Route B: conversational (attended)

SSE is one-way, so the open stream carries the approval prompt down to the client
and the decision must come back as a separate HTTP request. The tool call parks
in-process on a oneshot channel; the original stream stays open the whole time.

```
Client ──POST /v1/chat/completions────────► aura-web-server
   │                                           │ agent loop hits gate
   │ ◄══ SSE stream (STAYS OPEN) ════════════  │
   │     aura.approval_pending                 ▼
   │     {decision_id, args, expires_at}    registry.register(decision_id)
   │                                        tool call PARKED on oneshot ◄──┐
   │  human reads prompt in client UI             (stream stays open)      │
   │                                                                       │
   └──POST /v1/approvals/{decision_id}──► ingress ── registry.resolve ─────┘
      {"approved": false, "reason": "…"}       │
   ◄══ SAME SSE stream continues ══════════════▼
       aura.approval_completed ... denial fed to LLM ... stream finishes
```

The park is exactly as durable as the SSE connection. A dropped stream cancels the
pending approval and the tool call fails closed. Durable park-and-resume (stream
closes, decision arrives later, run resumes from a checkpoint) is the State-ADR-gated
successor, not this work.

### Route B': turn-ending tool cycle (the OpenAI idiom), where it reaches

The standard conversational approval pattern ends the turn instead of holding it
open: the model emits a tool call, the stream closes with `finish_reason:
"tool_calls"`, the client runs the tool (and asks the human), and the decision
returns as a tool-result message in the next request.

Aura already runs this cycle for client-side tools: advertised tools register
as `PassthroughTool`s whose marker result ends the stream with `finish_reason:
"tool_calls"` (`streaming/handlers.rs:838`, `:1279`); the CLI executes locally and
re-POSTs the full history with the tool result; the stateless server resumes the
loop from history (`handlers.rs:729`). The agent-requested approval surface rides
this path unchanged: when a single-agent client advertises a `request_approval`
tool, approval is just a client tool, and any OpenAI-compatible consumer with
function calling can be an approver. See "Decision: approval tool attachment".

The cycle does not reach two places, and both fall back to route B:

1. **The config gate.** The LLM called the gated tool, not an approval tool. Ending
   the turn means synthesizing an approval tool call and, on resume, executing the
   original call from client-resent history. The client would hold the approved
   arguments between turns, which is the #191 tamper/binding problem. The registry
   keeps the parked call in server memory, where the arguments cannot change
   between approval and execution.
2. **Orchestration.** Client tools are dropped in orchestration (`builder.rs:1509`)
   because workers run nested agent loops server-side; ending the top-level turn
   mid-worker means parking the whole run (DAG state, wave progress, worker
   conversations), which is durable parking, which is the State ADR.

The server-side variants of the turn-ending pattern (OpenAI Assistants
`requires_action` + `submit_tool_outputs`, MCP elicitation, A2A `input-required`)
all park server state. The held-open stream is the degenerate in-memory park: the
same shape with no durable state. When the State ADR and #191 binding land, the
gate and orchestration converge onto the turn-ending cycle too, and the `DecisionId`
and ingress endpoint carry over unchanged; what changes is where the park lives.

## Decision: approval tool attachment

The agent-requested surface resolves by capability advertisement, the same
precedent as `--enable-client-tools`:

- Single-agent request whose client advertises a `request_approval` tool: the
  client's tool is attached (passthrough, turn-ending cycle). The server-side
  `RequestApprovalTool` is not attached; the approval happens client-side and no
  registry entry exists.
- Otherwise: the server-side `RequestApprovalTool` is attached and dispatches
  through the configured `DecisionRoute`.

Advertising the tool IS the attendance signal for this surface. The config gate
ignores the advertisement and always uses `DecisionRoute` (see above). The server
still emits `approval_requested`/`approval_completed` events around client-side
approvals for observability parity.

## Decision: approval lifecycle as a state machine

```
            ┌────────────┐  resolve(id, decision)   ┌──────────────────────┐
  register──►  Pending    ├─────────────────────────►  Decided(Approved|    │
            │ (in registry)│                          │          Denied{..}) │
            └─────┬───┬───┘                           └──────────────────────┘
                  │   └── deadline ──► TimedOut
                  └────── stream drop / shutdown ──► Cancelled(reason)
```

```rust
pub enum ApprovalDecision { Approved, Denied { reason: Option<String> } }

pub enum ApprovalOutcome {
    Decided(ApprovalDecision),
    TimedOut { waited: Duration },
    Cancelled(CancelReason),        // ClientDisconnected | Shutdown
}
```

Fail-closed is structural: three of the four terminal states deny. The only path to
executing the gated tool is `Decided(Approved)`. Timeout and disconnect stay distinct
from a human denial because they emit different events and carry different semantics
for any future retry logic.

`ApprovalOutcome` models terminal states only — Pending is not a variant. The
pending state is the suspended await itself (registry entry, oneshot, open stream),
so the only operation on it is awaiting it, and a match on `ApprovalOutcome` is
always a match on a finished approval.

## Decision: core domain types

Full signatures live in the module tree below; the ones that fix the design:

```rust
// decision.rs — the resolvable handle. Private field; generate()/parse() only.
// This is where #191's durable consumption/expiry semantics attach later.
// Derives Ord: UUID v7 sorts by creation time, so the registry's BTreeMap
// iterates pending approvals oldest-first.
pub struct DecisionId(Uuid);                 // Uuid::now_v7()

// decision.rs — WHO is asking (embedded in events and the webhook payload)
pub enum AgentScope {
    Single      { session_id: Option<SessionId> },
    Worker      { run_id: RunId, task: TaskIdentity, session_id: Option<SessionId> },
    Coordinator { run_id: RunId },           // future coordinator surface, declared now
}

// decision.rs — WHY this approval exists. Replaces the prototype's RequestType plus
// the matched_pattern: Option<String> field whose Some/None tracked the surface by
// convention. Exhaustive: a WorkerEscalation branch arrives with the
// coordinator-mediated work.
pub enum ApprovalOrigin {
    ConfigGate     { matched_pattern: String },   // display form of the glob that fired
    AgentRequested { reason: String },
}

// decision.rs — typestate for the parked call. outcome() consumes self, so a
// registration is awaited at most once; select! over rx / deadline / cancellation.
pub struct AwaitingDecision { /* id, oneshot::Receiver, deadline */ }
impl AwaitingDecision {
    pub async fn outcome(self, cancel: &CancellationToken) -> ApprovalOutcome;
}

// registry.rs — Clone newtype over an Arc, the SharedTaskStore idiom (see
// "Decision: where cross-request state lives"). Not a global static.
#[derive(Clone)]
pub struct PendingApprovals(Arc<PendingApprovalsInner>);

struct PendingApprovalsInner {
    // std::sync::Mutex: every operation is a synchronous map op (insert/remove/
    // oneshot send); nothing awaits while holding the lock.
    // BTreeMap keyed on DecisionId (UUID v7, time-ordered), so iteration is
    // chronological registration order: oldest pending approval first.
    entries: std::sync::Mutex<BTreeMap<DecisionId, PendingEntry>>,
}

// Each entry splits into a serialization-ready core and a runtime-only handle.
// Nothing is serialized today; the split means durable parking (State ADR) can
// persist ParkedApproval as-is, because it already carries everything needed to
// re-render and re-validate the approval after a restart. Deadlines are wall-clock
// Timestamps, not Instants — an Instant is meaningless across a process restart.
struct PendingEntry {
    parked: ParkedApproval,                  // serializable when the time comes
    wake: oneshot::Sender<ApprovalDecision>, // runtime-only, never serialized
}

pub struct ParkedApproval {
    pub request: ApprovalRequest,            // decision_id, request_id, agent scope,
                                             //   origin, items — the full payload
    pub registered_at: Timestamp,
    pub expires_at: Timestamp,
}

impl PendingApprovals {
    pub fn register(&self, request: ApprovalRequest, timeout: Duration) -> AwaitingDecision;
    pub fn resolve(&self, id: &DecisionId, d: ApprovalDecision) -> Result<(), ResolveError>;
    pub fn cancel_request(&self, request_id: &RequestId);
}

// route.rs — closed two-variant enum. The prototype's ApprovalDispatch trait is removed:
// the variant set is known, CLI standalone is just Conversational against its own
// registry, and decide() holds the shared semantics (deadline, fail-closed mapping,
// event emission) in one place instead of per-impl.
pub enum DecisionRoute {
    Conversational { registry: PendingApprovals, timeout: Duration },
    Webhook        { client: WebhookClient, timeout: Duration },
}
```

`resolve` removes the entry and the oneshot consumes itself, so a `DecisionId`
resolves at most once in-process. That is the state-free version of the #191
exactly-once invariant; the durable version slots in behind the same type.

`ApprovalRequest.request_id` is the global request id (the one used for SSE routing
and MCP cancellation). The prototype's two per-call `Uuid::new_v4()` sites are deleted.

## Decision: where cross-request state lives

`PendingApprovals` is the first mutable state on the chat path that crosses request
boundaries: an approval is registered during one request's stream and resolved by a
`POST /v1/approvals/{id}` that arrives as a different request. The existing chat-path
registries do not cross requests — the cancellation registry and the tool event
broker register and consume within a single request's lifecycle (their global statics
solve Rig's thread-jumping, not request-crossing).

The in-tree precedent for request-crossing state is the A2A server:
`SharedTaskStore(Arc<InMemoryTaskStore>)` is a `Clone` newtype over an `Arc`,
constructed once in `main` and captured by the request handler, and
`task_cancel_state: Arc<Mutex<HashMap<…>>>` in `AuraAgentExecutor` lets a
`tasks/cancel` request mutate state created by an earlier `message:send`.

`PendingApprovals` follows the same shape and lives on `AppState`, because both
chat-path handlers that touch it already receive `State<Arc<AppState>>`:

```rust
// aura-web-server/src/types.rs
pub struct AppState {
    // … existing fields unchanged …
    /// Cross-request HITL state: approvals parked by a streaming request,
    /// resolved by POST /v1/approvals/{decision_id} on a later request.
    /// Per-process; a decision must land on the pod that parked the call.
    pub pending_approvals: PendingApprovals,
}
```

Flow of ownership: `main` constructs one `PendingApprovals` → `AppState` → the
completions handler clones it into `AgentConfig` (alongside `request_id`) → the
builder/orchestrator construct `DecisionRoute::Conversational` with it → the ingress
handler resolves through the same instance via `State`. The CLI in standalone mode
constructs its own instance. If A2A grows HITL support, the executor receives a
clone of the same instance — one process, one registry. No global static is added.

## Decision: crate boundary is a DTO pattern

HITL types need an ownership rule across crates: the spike left `RequestType` and
`ApprovalDecision` in `aura-events` (an SSE wire crate) while the logic that owns
them lives in `aura`, and `aura-config` cannot hold the config types because the
crate dependency points the wrong way (#174). The rule:

- **`aura::hitl` owns the domain.** `AgentScope`, `ApprovalDecision`,
  `ApprovalOutcome`, `DecisionId`, `ApprovalOrigin`, `Timestamp` are defined there.
  Domain logic never imports its core types from `aura-events`.
- **`aura-events` is a serde-only DTO layer**: mirror structs for SSE events with
  string ids and RFC3339 timestamps, no behavior. `ApprovalDecisionWire` already
  follows this pattern; this applies it consistently.
- **`aura::hitl::events` is the single conversion boundary** (`impl From<&domain>
  for aura_events::XWire`). It is the only file that sees both worlds.
- **Parse-time config types** (`HitlConfig`, `DecisionRouteConfig`, `WebhookUrl`,
  `GlobPattern`) are defined in `aura` and marked `// TODO(#174): move to
  aura-config` for the dependency inversion.

The cost is mirror-struct duplication. The benefit is that the domain can evolve
without touching the wire crate, and clients consume DTOs that never drag domain
behavior with them.

## Decision: module tree replaces the god module

The spike's `crates/aura/src/hitl.rs` is a single 832-line module holding the
webhook wire structs, the HTTP client, the tool wrapper, the agent tool, and event
emission. It splits:

```
crates/aura/src/hitl/
├── mod.rs        // facade: pub use the public surface, nothing else
├── protocol.rs   // webhook wire: ApprovalRequest, ApprovalItem, version const
├── decision.rs   // domain core: DecisionId, ApprovalDecision, ApprovalOutcome,
│                 //   ApprovalOrigin, AgentScope, CancelReason, AwaitingDecision
├── registry.rs   // PendingApprovals, PendingEntry, ResolveError
├── route.rs      // DecisionRoute + webhook client
├── events.rs     // From<&domain> for aura_events DTOs (only file importing both)
├── gate.rs       // HitlApprovalWrapper (config-gate surface)
└── tool.rs       // RequestApprovalTool (agent-callable surface)
```

## Decision: config schema

```toml
[hitl]
require_approval = ["kubectl_*", "restart_*"]

[hitl.route]
mode = "conversational"      # or: mode = "webhook", url = "https://…"
timeout_secs = 300           # per-route defaults differ
```

```rust
pub struct HitlConfig {
    #[serde(default)]
    pub require_approval: Vec<GlobPattern>,    // pre-compiled globset at TOML load
    pub route: DecisionRouteConfig,            // required when [hitl] is present
}

#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DecisionRouteConfig {
    Conversational { timeout_secs: u64 },            // default 60: the approver is
                                                     //   already at the client
    Webhook { url: WebhookUrl, timeout_secs: u64 },  // default 300: the webhook may
                                                     //   page a human or route
                                                     //   through chat ops
}
```

`Option<HitlConfig>` on `Config` remains the enable bit; there is no `enabled` bool.
The "webhook_url required, never empty" invariant holds structurally: the
`Webhook` variant cannot parse without a valid URL. The `matched_pattern` on the wire
is the original pattern string, so `GlobPattern` keeps its source text alongside the
compiled matcher.

Config validation warns when either route's timeout is greater than or equal to
`per_call_timeout_secs` in orchestration mode: a parked tool call that outlives its
task budget gets killed by the wrong mechanism.

## Decision: SSE events and the ingress endpoint

```rust
// aura-events DTOs
ApprovalRequested { decision_id, tool_name, origin, scope }
ApprovalPending   { decision_id, tool_name, arguments, origin, scope, expires_at }
ApprovalCompleted { decision_id, outcome, duration_ms, scope }   // outcome includes
                                                                 // timeout/cancelled
```

`aura.approval_pending` emits only on the conversational route. It is the attended
prompt: `decision_id` is the resolution handle, `expires_at` lets a client render a
countdown. The webhook route keeps the requested/completed pair.

```
POST /v1/approvals/{decision_id}
body: { "approved": true } | { "approved": false, "reason": "…" }
204 on success · 404 unknown/expired/already-resolved · 400 on parse
```

The body mirrors the webhook response format, so an approver service and an attended
client speak the same decision schema.

## Decision: orchestration behavior

Workers share the parent's request id, so `approval_pending` events from a parked
worker reach the client on the request's stream, and a decision posted to the ingress
resolves the worker's oneshot directly. Multiple workers can park concurrently under
one request id; each has its own `DecisionId`, so the registry keys on decisions
rather than requests. Single-agent mode is the degenerate case on the same
code path. The `Coordinator` scope variant and `WorkerEscalation` origin are declared
design space for future coordinator-mediated mechanisms; neither is constructed in this
work.

## Alternatives considered

- **Webhook as the only decision channel** (the spike's model). Cannot serve an
  attended session: SSE is one-way and there is no endpoint a decision can come
  back through, so the operator in the chat — the primary approver for dogfooding —
  can never answer. Kept as the unattended route, rejected as the whole story.
- **Turn-ending tool cycle for every surface.** The cleanest OpenAI-compatible
  shape, and it is the design for the agent-requested surface in single-agent. It
  does not reach the config gate (the client would hold the approved arguments
  between turns — the #191 tamper problem) or orchestration (ending the top-level
  turn mid-worker parks the whole run, which is durable parking, #209). See "Route
  B': turn-ending tool cycle".
- **Runtime attendance detection** (an `Auto` route). Rejected: the deployment
  already knows whether it is attended, so routing is pure configuration. Revisit
  only if one deployment ever serves both shapes at once.
- **An open dispatch trait** (the spike's `ApprovalDispatch` + `Arc<dyn>`).
  Rejected for a closed two-variant enum: the variant set is known, and the shared
  semantics (deadline, fail-closed mapping, event emission) belong in one place,
  not per-impl.
- **Binding and exactly-once now** (#191). The durable versions need state storage
  that does not exist yet, so they are deferred to the State ADR (#209). The
  `DecisionId` model is shaped so that work attaches later without reworking
  callers (see "Decision: core domain types").

## Keep / rework / discard map for the prototype branch

| Prototype element | Verdict | Notes |
|---|---|---|
| `HitlApprovalWrapper` gate, composed first in the chain | keep | insertion point unchanged |
| `RequestApprovalTool` surface | keep | constructs `ApprovalOrigin::AgentRequested`; attached only when the client does not advertise its own `request_approval` |
| Webhook HTTP client + fail-closed error handling | keep | becomes the `Webhook` route arm |
| `request_approval` excluded from glob matching | keep | |
| SSE routing via `tool_event_broker` + request id | keep | |
| Unit tests | mostly keep | assertions updated only where the compiler forces it |
| `hitl.rs` single file | rework | module tree above |
| `ApprovalRequest` / `ApprovalItem` shape | rework | `decision_id`, `origin`, scope on agent; `task` and per-item `matched_pattern` removed |
| `HitlConfig` | rework | route enum replaces `enabled` + `webhook_url` |
| Hand-rolled `glob_match` | rework | `globset`, compiled at TOML load |
| `HitlContext` | rework | carries `DecisionRoute`; constructor instead of literal struct syntax |
| Duplicated `ApprovalRequest` construction (wrapper vs tool) | rework | unified in `route.rs` |
| `ApprovalDispatch` trait + `Arc<dyn>` | discard | closed enum replaces it |
| Per-call `Uuid::new_v4()` request ids (hitl.rs:402, 534) | discard | global request id |
| `RequestType` enum | discard | `ApprovalOrigin` replaces it |
| `HitlConfig.enabled` + empty-URL checks | discard | |

## Consequences

- HITL is additive, opt-in config: no `[hitl]` table, no behavior change. Example
  configs and `docs/hitl-integration-guide.md` update in the same PR.
- A conversational park survives only as long as the SSE connection. Flaky client
  connections deny approvals. This is the accepted V-next contract; durable parking
  arrives with the State ADR.
- The registry is per-process. A decision must land on the pod that parked the call.
  Single-pod deployments only until the State ADR; this is the named boundary.
- Neither the decision ingress nor the webhook egress authenticates: the server has
  no auth layer today, so possession of a `decision_id` is the only capability needed
  to resolve an approval. This is a named gap for the roadmap workstream. The likely
  shape is forwarding a request header into the approval flow the way MCP
  `headers_from_request` routing works, or an OAuth-backed approver identity;
  details TBD.
- The conversational route holds streams open while a call parks: the connection is
  intentionally idle, so an intermediary dropping it denies the approval. SSE
  keep-alive heartbeats during the park matter for attended usability, and
  long-silent-call detection ([#187](https://github.com/mezmo/aura/issues/187))
  must treat a parked approval as alive, not stuck.
- An async webhook (webhook replies 202, posts the decision to `/v1/approvals/{id}`
  later) is route A's egress plus route B's ingress and registry. Nothing in this
  design blocks it; it is deferred with the other state-gated work.
- Timeout defaults: 60s conversational (the approver is at the client), 300s
  webhook (the webhook may route through paging or chat ops; unattended approval
  is not realistically a 30-second operation). Both interact with
  `per_call_timeout_secs` in orchestration mode; the config validation warning
  covers both routes.
- A client-side `request_approval` result rides the client-resent history like any
  other tool result. In attended mode the client human is the approver, so the
  client is trusted with its own decision; tamper-evidence (args digest, HMAC) is
  part of the deferred #191 binding work.

## Out of scope

Durable parking/resume, A2A `TaskState::InputRequired` wiring, confidence-based
auto-approve, multi-webhook fan-out and runtime webhook registration, an `Auto`
route variant (the only feature that would need runtime attendance detection),
authentication on the decision ingress (see Consequences), the coordinator-mediated
approval surfaces (a plan-review routing tool on the coordinator, and worker
escalation routed through it — the `Coordinator` scope and `WorkerEscalation`
origin declared above are their reserved design space), and the durable forms
of the #191 binding/exactly-once invariants.

## Implementation phasing

Three phases on the existing branch, squashed for review per phase. Workspace
build/test/clippy per commit; integration tests where behavior changes; an
end-to-end smoke at each phase boundary — an aura TOML using the `[hitl]` schema
above (gate pattern plus the phase's route) against a local mock webhook.

1. **Domain + module split.** The module tree, domain types, DTO boundary,
   `DecisionRoute` with the webhook arm only, config route enum, `globset`. Wire and
   TOML breaks land here. Behavior matches the prototype: webhook route, same
   fail-closed paths.
2. **Conversational route.** `PendingApprovals` in `AppState`, ingress endpoint,
   `approval_pending` event, disconnect/shutdown cancellation wiring.
3. **Attended client + orchestration verification.** CLI ships a local
   `request_approval` tool (turn-ending cycle, single-agent) and renders
   `approval_pending` + posts to the ingress (held-stream, orchestration and the
   gate). Orchestration integration test: parked worker, decision via ingress,
   wave continues. Docs update.
