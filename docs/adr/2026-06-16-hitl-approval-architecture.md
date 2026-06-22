<!-- markdownlint-disable MD033 -->
# Human-in-the-loop approval gating for agent tool calls

- Status: **accepted**
- Deciders: Mike Shearer
- Date: 2026-06-16

Technical Story: [#191](https://github.com/mezmo/aura/issues/191)

## Context and Problem Statement

Aura's agents execute tool calls unconditionally. There is no way to require a
human to sign off before a destructive call runs, which blocks SRE dogfooding of
the orchestrated agent ([#191](https://github.com/mezmo/aura/issues/191)):
incident remediation needs an operator's approval before anything irreversible
happens.

Two deployment shapes need approval, and they differ structurally:

- **Unattended** (A2A, background): no human is on the stream, so an external
  service answers approval requests.
- **Attended** (CLI, dev, dogfooding): the operator chatting with the agent over
  SSE is the approver. SSE is server-to-client only, so the prompt can ride the
  stream down, but the decision needs a path back up, and the server exposes no
  endpoint that accepts one.

A spike of the webhook half lives on the unmerged branch
`mshearer/hitl-v1-config-gate` (@ `52f37e6`). It validated the gate insertion
point, both approval surfaces, and the fail-closed policy. This decision reworks
that spike; the companion design note (see Links) carries the type-level detail
and a keep / rework / discard map over the spike.

## Decision Drivers <!-- optional -->

- SRE dogfooding needs an operator to approve irreversible remediation before it runs ([#191](https://github.com/mezmo/aura/issues/191)).
- Both deployment shapes (attended and unattended) have to be served by one model.
- The deployment already knows whether it is attended, so routing should be configuration rather than runtime detection.
- No durable cross-request state exists yet; the design must not depend on it ([#209](https://github.com/mezmo/aura/issues/209)).
- The approved arguments must reach execution without the chance to be tampered with between approval and the call.

The approval gate **MUST** fail closed: any outcome other than an explicit human
approval (denial, timeout, disconnect, shutdown) results in the gated tool not
running.

The attended route **MUST** be answerable by the operator already on the SSE
stream, without a second service standing by.

Routing **SHOULD** be fixed per deployment in configuration. Runtime attendance
detection is only warranted if a single deployment ever serves both shapes at
once, which none does today.

The chosen design **SHOULD** keep the `DecisionId` and decision-ingress contract
forward-compatible, so the durable binding and exactly-once guarantees deferred
to [#209](https://github.com/mezmo/aura/issues/209) attach later without
reworking callers.

## Considered Options

- Webhook as the only decision channel (the spike's model)
- Turn-ending tool cycle for every surface (the OpenAI idiom)
- Dual-channel chosen by config: webhook for unattended, conversational held-stream for attended
- Runtime attendance detection (an `Auto` route)

## Alternative Options

Considered but not evaluated in depth, because they are secondary to the routing
decision and slot in behind the same types later:

- An open dispatch trait (`Arc<dyn ApprovalDispatch>`) instead of a closed route enum.
- Implementing the durable argument-binding and exactly-once guarantees now ([#191](https://github.com/mezmo/aura/issues/191)) rather than deferring them.

## Decision Outcome

Chosen option: **dual-channel chosen by config**. Approval requests travel one of
two routes, fixed per deployment in a `[hitl]` TOML table. There is no runtime
attendance detection.

- **Route A (webhook), unattended.** One synchronous HTTP round-trip. The gate
  posts the approval request to a configured service and blocks up to a timeout;
  the decision comes back in the response body.
- **Route B (conversational), attended.** The open SSE stream carries the prompt
  down to the client; the tool call parks in-process on a oneshot channel while
  the original stream stays open; the decision returns as a separate
  `POST /v1/approvals/{decision_id}`. The park is exactly as durable as the SSE
  connection: a dropped stream cancels the pending approval and the call fails
  closed.

Where the standard OpenAI turn-ending tool cycle already runs (a single-agent
client that advertises its own `request_approval` tool), the attended surface
reuses it unchanged, so approval is an ordinary client tool that any
OpenAI-compatible consumer can answer. The config gate and orchestration both
fall back to Route B. Ending the turn at the gate would let the client hold
approved arguments across the gap (the [#191](https://github.com/mezmo/aura/issues/191)
tamper problem); ending it mid-worker in orchestration would park the entire run
(durable state, [#209](https://github.com/mezmo/aura/issues/209)). The detail of
where each surface lands lives in the design note.

The approval lifecycle has four terminal states: `Approved`, `Denied`,
`TimedOut`, and `Cancelled`. Only `Approved` executes the gated call, so three of
the four states deny and fail-closed is structural rather than a policy check.

The cross-request registry that parks conversational approvals is per-process and
lives on the web server's shared state. A decision must land on the same process
that parked the call. Durable park-and-resume across processes is the
[#209](https://github.com/mezmo/aura/issues/209)-gated successor, not this work.

Approval SSE events (`approval_requested`, `approval_pending`,
`approval_completed`) emit regardless of the `AURA_CUSTOM_EVENTS` flag. They are
protocol the client has to act on (the attended prompt and the decision record),
not optional observability, so they get the same always-on treatment as the
existing client-tool turn-ending chunks rather than riding the gated custom-event
channel.

### Positive Consequences <!-- optional -->

- HITL is additive and opt-in. With no `[hitl]` table there is no behavior change.
- One model serves both the attended and unattended deployment shapes.
- Fail-closed is enforced by the type that represents the outcome, not by a
  reviewer remembering to check.
- The `DecisionId` and decision-ingress contract are shaped so the deferred
  binding ([#191](https://github.com/mezmo/aura/issues/191)) and durable parking
  ([#209](https://github.com/mezmo/aura/issues/209)) attach without reworking callers.

### Negative Consequences <!-- optional -->

- A conversational park survives only as long as the SSE connection. Flaky client
  connections deny approvals.
- The registry is per-process, so a decision must reach the process that parked
  the call. Deployments stay single-pod until [#209](https://github.com/mezmo/aura/issues/209).
- Neither the decision ingress nor the webhook egress authenticates. The server
  has no auth layer today, so possession of a `decision_id` is the only
  capability needed to resolve an approval. This is a named gap for the roadmap.
- Holding the stream open during a park keeps an intentionally idle connection
  alive, so long-silent-call detection ([#187](https://github.com/mezmo/aura/issues/187))
  must treat a parked approval as alive, not stuck, and an intermediary dropping
  the idle stream denies the approval.
- The route timeouts interact with `per_call_timeout_secs` in orchestration mode;
  a park that outlives its task budget is killed by the wrong mechanism.

## Pros and Cons of the Options <!-- optional -->

### Dual-channel chosen by config

- Good, serves the attended operator (the primary dogfooding approver) and the unattended service with one model.
- Good, routing is pure configuration; the deployment already knows its shape.
- Good, the webhook arm is the validated spike behavior, carried over directly.
- Bad, two routes to build and document instead of one.
- Bad, the conversational route is only as durable as the SSE connection until [#209](https://github.com/mezmo/aura/issues/209).

### Webhook as the only decision channel

- Good, the simplest model, and it is exactly the validated spike.
- Bad, it cannot serve an attended session: SSE is one-way and there is no
  endpoint a decision can return through, so the operator in the chat (the
  primary dogfooding approver) can never answer.

Kept as Route A, rejected as the whole story.

### Turn-ending tool cycle for every surface

- Good, the cleanest OpenAI-compatible shape, and it is the design for the
  attended single-agent surface.
- Bad, it does not reach the config gate: the client would hold the approved
  arguments between turns, which is the [#191](https://github.com/mezmo/aura/issues/191) tamper problem.
- Bad, it does not reach orchestration: ending the top-level turn mid-worker
  parks the whole run (DAG state, wave progress, worker conversations), which is
  durable parking, which is [#209](https://github.com/mezmo/aura/issues/209).

### Runtime attendance detection (an `Auto` route)

- Good, the system would work from the presence of a client stream, with no
  per-deployment route configuration to set, which is the better developer experience.
- Bad, the deployment already knows whether it is attended, so detecting it at
  runtime adds machinery for a question configuration already answers, and a
  misread (a slow attended client that looks unattended) routes to the wrong
  channel. Revisit only if one deployment ever serves both shapes at once.

## Links <!-- optional -->

- Design and implementation note: [docs/design/hitl.md](../design/hitl.md) (domain types, module layout, config schema, wire formats, and the keep / rework / discard map over the spike)
- Refines [#191](https://github.com/mezmo/aura/issues/191) (HITL approval gates for MCP tool calls)
- Depends on [#209](https://github.com/mezmo/aura/issues/209) (durable cross-request state, the State ADR) for durable parking, binding, and exactly-once
- Interacts with [#187](https://github.com/mezmo/aura/issues/187) (long-silent-call detection must treat a parked approval as alive)
- Builds on the config-crate inversion from [#174](https://github.com/mezmo/aura/issues/174) / [PR #201](https://github.com/mezmo/aura/pull/201)
- Reworks the spike on branch `mshearer/hitl-v1-config-gate` (@ `52f37e6`)
- RFC 2119: <https://www.rfc-editor.org/rfc/rfc2119>
