<!-- markdownlint-disable MD033 -->
# Poll the approval webhook for parked decisions instead of receiving a callback

- Status: **proposed**
- Deciders: Tony Rogers
- Date: 2026-09-03
- Amends: [2026-07-21-hitl-park-reify.md](2026-07-21-hitl-park-reify.md)
  (decisions 5, 8, 10, 12, 13) and the Route A definition in
  [2026-06-16-hitl-approval-architecture.md](2026-06-16-hitl-approval-architecture.md)

Technical Story: [#271](https://github.com/mezmo/aura/issues/271) (HITL
Park/Reify); polling tracked by: unfiled

## Context and Problem Statement

The HITL architecture ADR defines two decision routes. Route A (webhook) is one
synchronous HTTP round trip: aura POSTs the approval request to a receiver and
blocks up to `timeout_secs` for the decision in the response body. Route B
(conversational) parks the call and receives the decision through the
**decision ingress**, `POST /v1/approvals/{decision_id}` on the aura web
server.

The park/reify ADR makes parking durable. It does so only for Route B: the
webhook route "resolves in one round trip and keeps no parked records", and
the only durable wake path is a decision landing at the decision ingress. A
webhook receiver that wants to answer after the request has ended therefore
has exactly one option: call back into aura's decision ingress.

That callback works when the receiver and the agent share a network. It fails
for the deployment this ADR exists for: a hosted governance service acting as
the approval receiver for an aura agent running inside a customer's
environment. The customer's agent can dial out to the governance service; the
governance service cannot dial in to the agent without the customer exposing
aura publicly, which is not an acceptable ask.

The synchronous Route A already has the right network direction, since every
byte flows outbound from aura. Its problem is duration, not direction: a
human approval window measured in hours cannot ride a single HTTP request
bounded by `STREAMING_TIMEOUT_SECS` ([#613](https://github.com/mezmo/aura/issues/613)).

This decision makes Route A parkable and defines how a parked webhook approval
learns its decision without any inbound path to aura.

## Decision Drivers <!-- optional -->

- Every leg of the webhook route **MUST** be initiated by aura. The receiver
  **MUST NOT** need a route to the agent.
- The fail-closed guarantee **MUST** hold across the new path: a decision
  fetch that fails, times out, or fails verification leaves the run parked;
  only the expiry reaper (park/reify decision 12) denies.
- A polled decision **MUST** be authenticated at least as strongly as a
  synchronous Route A response. It arrives long after the request that
  created it, so possession of a `decision_id` alone **MUST NOT** be enough
  to approve a call.
- Two pods fetching the same decision **MUST** be safe. Park/reify decision 8
  (durable, first-write-wins resolution) is the mechanism; this ADR **MUST
  NOT** introduce a second one.
- The poller **MUST NOT** become a standing per-run subscriber, the shape
  park/reify decision 5 rejected. It **SHOULD** be a per-pod scanner with the
  same lease-and-fencing discipline as the reaper.
- The synchronous fast path **MUST** keep working unchanged for receivers that
  can answer inside the request.
- The conversational route and the decision ingress **MUST** be unaffected.
  Attended deployments keep the ingress; this ADR adds nothing they have to
  adopt.
- Wake latency **SHOULD** be close to the callback design's. A human who
  approves should not wait a full poll interval to see the run move.

## Considered Options

- **A. Decision ingress callback** (the implicit status quo once Route A
  parks): the receiver POSTs the decision to `POST /v1/approvals/{decision_id}`.
- **B. Hold the synchronous request longer**: raise the webhook and stream
  timeouts to cover the approval window.
- **C. Per-decision polling**: aura GETs one URL per outstanding decision on a
  fixed interval.
- **D. Batched long-poll**: aura POSTs the set of outstanding decision ids it
  holds; the receiver holds the request until one resolves or a wait budget
  expires.
- **E. Outbound persistent stream**: aura opens an SSE or WebSocket connection
  to the receiver and decisions are pushed down it.
- **F. Receiver-supplied poll URL**: the pending response carries the URL aura
  should poll next.

## Decision Outcome

Chosen option: **D, batched long-poll**, with the pending response as the
park trigger. The sections below are numbered for reference and continue the
park/reify ADR's numbering where they amend it.

```mermaid
sequenceDiagram
    participant S as aura (customer env)
    participant R as governance receiver (hosted)
    Note over S: worker hits gate
    S->>R: POST {url} approval request (signed: approval-request:{decision_id})
    R-->>S: 202 {"status":"pending"} (signed: approval-decision:{decision_id})
    Note over S: approval parks durably; run drains to wave boundary and parks
    loop poller, every pod, interval poll_interval_secs
        S->>R: POST {poll_url} {poll_id, instance_id, decision_ids:[...]} (signed: approval-poll:{poll_id})
        Note over R: holds up to poll_wait_secs, returns early on any decision
        R-->>S: 200 {poll_id, decisions:[{decision_id, approved, reason}]} (signed: approval-poll-response:{poll_id})
    end
    Note over S: resolve (first-write-wins) -> claim session -> reify
```

### 19. The pending response makes Route A parkable

A receiver answers a Route A request in one of two ways:

- `200` with the existing decision body `{"approved": bool, "reason": ...}`.
  The call resolves in-request exactly as today.
- `202 Accepted` with the body `{"status": "pending"}`. The gate treats this
  as a durable park of that approval, identical to a conversational park:
  the approval record persists (park/reify decision 8), the task enters
  `Blocked { decision_id }`, and the run drains to a quiescent wave boundary
  (decision 1).

Both bodies are signed under the existing response context
`approval-decision:{decision_id}` and verified before parsing. Any other
status stays a channel fault (`BadStatus`) and denies in-request, as today.
A `202` is only honored when a webhook HMAC secret is configured; without
one, a `202` is a channel fault and the call denies without parking (see 22).

The `timeout_secs` field keeps its meaning as the bound on the synchronous
leg. The park window is `[hitl] park_timeout_secs`, shared with the
conversational route.

### 20. Decisions are fetched by aura, never delivered to it

Aura obtains parked webhook decisions by polling a receiver-hosted endpoint.
The receiver never contacts aura. The decision ingress remains the wake path
for the conversational route and is not used by the webhook route at all.

The poll is a `POST` to a configured `poll_url`, with a signed JSON body:

```json
{
  "version": 1,
  "poll_id": "<uuid>",
  "instance_id": "<uuid>",
  "decision_ids": ["<uuid>", "..."],
  "wait_secs": 30
}
```

`instance_id` is the stable per-process identity defined in
[#495](https://github.com/mezmo/aura/issues/495) (UUIDv5 over an environment
and config fingerprint). It lets the receiver key its outstanding set by
agent instance and correlate polls with the approval requests that created
them. That identity is not yet on the approval request payload on `nightly`;
landing it there is a prerequisite, and the poll carries the same value.

The receiver **MAY** hold the request for up to `wait_secs` and **MUST**
return as soon as any listed decision has resolved. The response is:

```json
{
  "version": 1,
  "poll_id": "<same uuid>",
  "decisions": [
    { "decision_id": "<uuid>", "approved": true, "reason": null, "headers": {} }
  ]
}
```

Decision ids the receiver has not resolved are omitted. Decision ids the
receiver does not recognize are also omitted; they expire through the reaper
(decision 12) like any other unanswered approval, and the receiver gets no
way to deny a call it never saw.

Batching by instance rather than per decision bounds the poll rate by pod
count, not by outstanding approvals, and gives the receiver one place to see
everything an agent instance is waiting on.

### 21. The poller is the reaper's sibling, and the unattended wake actor

A background task on every pod scans the approval store for parked webhook
approvals and polls for them on `poll_interval_secs` (or continuously when
long-polling, since the wait budget is the interval). It runs under the same
lease-and-fencing rules as the P8 reaper and is idempotent by construction:
a decision landing twice, from two pods or a retried poll, hits the
first-write-wins resolve of decision 8 and the second write is a no-op.

Every pod polls for every outstanding webhook approval it can see. Duplicate
polls are bounded by pod count; electing a single polling pod is an
optimization deferred until the cost is measured.

Park/reify decision 5 says "the pod that receives the wake pulls and claims".
For the conversational route that pod is the one that served the decision
ingress. For the webhook route there is no inbound request, so **the poller
is the wake receiver**: on a resolved decision it records the wake reason,
then attempts the claim and reify on its own pod under decision 5's lease.
This is the one place decision 5 is extended rather than restated.

An unattended reified run has no SSE subscriber. Its lifecycle events publish
to the request-scoped broker as today, with no consumer, and its outcome is
retrievable by session handle per decision 15. Delivering the outcome
anywhere else (an A2A task update, a completion webhook) is a separate
decision and is not made here.

### 22. Polled decisions are signed, bound to the poll, and mandatory to sign

The poll request body is signed under `approval-poll:{poll_id}`; the response
body under `approval-poll-response:{poll_id}`, both with the existing
`X-Aura-Signature-256` and `X-Aura-Timestamp` headers, secret set, tolerance,
and raw-bytes rule ([#528](https://github.com/mezmo/aura/issues/528)). The
response **MUST** echo the request's `poll_id`; a mismatch is a verification
failure. Binding the response to a fresh per-poll id means a captured
response cannot be replayed into a later poll, on top of the per-decision
argument-digest binding of decision 9 that already limits what any decision
can release.

Unlike the synchronous leg, where an unset secret disables signing on both
sides and the receiver must independently insist on it, **a parked webhook
approval requires a configured secret**. A polled decision is the only
authorization for a call that may execute hours later on a different pod;
an unsigned one is not acceptable. Config validation rejects `poll_url`
without `AURA_HITL_WEBHOOK_SECRET`, and at runtime a `202` without signing
configured is a channel fault (decision 19).

### 23. Fail-closed boundaries for the fetch path

The following leave the run parked and are retried with backoff; none of them
denies:

- transport errors and non-`200` poll responses;
- signature or timestamp verification failures;
- malformed response bodies;
- a decision id absent from the response.

Only the reaper's expiry of `park_timeout_secs` denies, terminalizing the run
`Failed { cause: ParkExpired }` (decision 12). A receiver outage therefore
costs latency, never a spurious denial and never a spurious approval.

### 24. Approver identity on polled decisions

`tool_headers_from_response` (approver identity forwarding,
[2026-08-13](2026-08-13-approver-identity-forwarding.md)) reads header
overrides off the synchronous response's HTTP headers. A batched, body-signed
poll response has no per-decision HTTP headers, so each decision entry
carries an optional `headers` object and the same mappings apply to it.

Captured overrides on a parked approval follow park/reify decision 13's
classification. `identity`-classified values persist with the wake reason
and replay on reify. A `credential`-classified value on a polled decision is
refused: the decision resolves as denied with a reason naming the header,
because persisting a secret across the park is the outcome decision 13
exists to prevent. Receivers that need credential forwarding must answer
synchronously.

### 25. Abandonment is visible in the poll set

The `decision_ids` list is a natural heartbeat: a decision aura stops listing
has been consumed, expired, or abandoned, and the receiver may retract
whatever it showed a human. This narrows but does not close
[#613](https://github.com/mezmo/aura/issues/613): an explicit cancellation
POST is still the right answer for the synchronous leg and for prompt
retraction, and is not decided here.

### 26. Config surface

The `[hitl.route]` webhook variant gains:

| Field | Default | Meaning |
| --- | --- | --- |
| `poll_url` | none (polling off) | Receiver endpoint for decision fetches. Setting it enables `202` handling. |
| `poll_interval_secs` | `15` | Delay between polls when the previous poll returned without a decision. |
| `poll_wait_secs` | `30` | Long-poll hold budget sent as `wait_secs`; `0` requests an immediate answer. |

The client-side HTTP timeout for a poll is `poll_wait_secs` plus a fixed
margin. `poll_url` is a distinct, explicitly configured URL: it is never
derived from `url`, and it is never taken from a receiver response (option F,
rejected below).

With `poll_url` unset, a `202` is a channel fault and Route A behaves exactly
as it does today.

### Positive Consequences <!-- optional -->

- The hosted-receiver deployment works with no inbound connectivity to the
  customer's agent and no change to the customer's network posture.
- Webhook deployments need no decision ingress at all, removing the
  unauthenticated-ingress gap named in both prior ADRs from that deployment
  shape entirely.
- The synchronous Route A contract is preserved byte for byte; a receiver
  that never returns `202` is unaffected.
- Fail-closed stays structural: the fetch path has no code path that denies,
  so a fetch bug can only delay, never approve or deny.
- Idempotency and ownership reuse park/reify decisions 5, 8, and 12 rather
  than adding mechanisms.

### Negative Consequences <!-- optional -->

- Wake latency is bounded below by the poll round trip, and above by
  `poll_interval_secs` when the receiver does not long-poll.
- Every pod polls for every outstanding approval; cost grows with pod count
  until a polling leader is introduced.
- The poller is a second wake actor beside the decision ingress, and the only
  one for unattended runs. Decision 5's "pod that receives the wake" now
  includes a pod that received nothing from outside.
- Unattended reified runs complete into a store with nobody watching. Until
  an outcome-delivery decision lands, an operator learns the result only by
  retrieving the session.
- Credential-forwarding receivers cannot use parking (decision 24). Correct,
  and it will surprise operators until service identity (T1-D) exists.
- The receiver contract grows a second endpoint and a second pair of signing
  contexts that [#528](https://github.com/mezmo/aura/issues/528) must document.

## Pros and Cons of the Options <!-- optional -->

### A. Decision ingress callback

- Good: lowest latency, no new endpoint on aura, already built for Route B.
- Bad: requires the receiver to reach the agent, which the target deployment
  forbids; rejected as the webhook wake path.

### B. Hold the synchronous request longer

- Good: zero protocol change.
- Bad: ties an hours-long human decision to one TCP connection, one process,
  and one stream timeout; the run cannot park, and any intermediary that
  drops idle connections denies the call. Rejected.

### C. Per-decision polling

- Good: simplest receiver endpoint; each response is signed under the
  existing `approval-decision:{decision_id}` context.
- Bad: request rate scales with outstanding approvals times pods; no
  long-poll, so latency is a full interval. Rejected in favor of D, which
  degrades to this shape when `poll_wait_secs` is `0`.

### D. Batched long-poll (chosen)

- Good: outbound only; near-callback latency when the receiver long-polls;
  one request per pod per interval regardless of approval count; the id list
  doubles as an abandonment signal.
- Bad: a new signed body shape and two new signing contexts; per-decision
  identity headers move into the body.

### E. Outbound persistent stream

- Good: lowest latency of the outbound options.
- Bad: a standing connection per pod is the subscriber shape decision 5
  rejected, needs reconnect and replay semantics, and is harder to front with
  ordinary HTTP infrastructure. Deferred; D's contract can be carried over an
  SSE response later without changing the body shapes.

### F. Receiver-supplied poll URL

- Good: the receiver can shard or relocate its decision endpoint freely.
- Bad: lets a compromised or misconfigured receiver point the agent at an
  arbitrary URL from inside the customer network, a server-side request
  forgery surface for no operational gain. Rejected; `poll_url` is
  configuration only.

## Links <!-- optional -->

- Amends [2026-07-21-hitl-park-reify.md](2026-07-21-hitl-park-reify.md)
  (decisions 5, 8, 10, 12, 13; adds 19 to 26)
- Amends Route A in
  [2026-06-16-hitl-approval-architecture.md](2026-06-16-hitl-approval-architecture.md)
- Builds on [2026-08-13-approver-identity-forwarding.md](2026-08-13-approver-identity-forwarding.md)
  and the HMAC contract from [#399](https://github.com/mezmo/aura/issues/399)
- Instance identity on the webhook payload: [#495](https://github.com/mezmo/aura/issues/495)
- Receiver contract documentation: [#528](https://github.com/mezmo/aura/issues/528)
- Webhook cancellation contract: [#613](https://github.com/mezmo/aura/issues/613)
- Park/reify implementation: [#271](https://github.com/mezmo/aura/issues/271),
  PRs [#656](https://github.com/mezmo/aura/pull/656) and
  [#658](https://github.com/mezmo/aura/pull/658)
- Design note: [docs/design/hitl.md](../design/hitl.md)
- RFC 2119: <https://www.rfc-editor.org/rfc/rfc2119>
