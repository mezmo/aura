<!-- markdownlint-disable MD033 -->
# Forward the webhook approver's identity to the gated MCP call

- Status: **accepted**
- Deciders: Mike Shearer
- Date: 2026-08-13

Technical Story: [#496](https://github.com/mezmo/aura/issues/496)

## Context and Problem Statement

MCP client headers are resolved once per inbound chat request, at agent-build
time: the completions handler flattens the inbound `HeaderMap`, overlays it
per `headers_from_request` config, and the MCP client bakes the result into
its default headers. Identity is frozen before the LLM emits any tool call,
and before any HITL approval exists.

When a human approves a gated tool call, the downstream MCP call still
executes under the original requester's frozen identity, not the approver's.
An operator who wants the approver's own credentials (or a context header
naming them) on the call they just authorized has no way to get it there.

## Decision Drivers

- Forwarding **MUST** be config-selectable: an operator may map real auth
  material (`Authorization`) or context headers, since the security posture
  is designed to the auth worst case.
- The override **MUST** be scoped to exactly the one gated call the approval
  releases. Every tool execution re-enters `pre_call` and the gate mints a
  fresh `DecisionId` per call, so a retry **MUST** re-gate and re-capture;
  no reuse store exists.
- A missing or invalid captured header **MUST** fail the call closed, naming
  the affected header names and never a value.
- The route-wide `ApprovalOutcome` **MUST NOT** gain a credential-holding
  path: the agent-callable `request_approval` surface has no consumer for
  captured headers.
- Delivery **MUST NOT** require a fork or upgrade of the pinned `rmcp`
  client library.

## Considered Options

Delivering a per-call header override through to the outbound MCP POST,
verified against pinned `rmcp` 0.12.0:

- Parameter threading into `post_message`'s signature. Rejected: the
  signature belongs to a trait `rmcp` owns.
- A task-local read inside the transport worker. Rejected: the worker task
  is spawned once at connect time, before any gated call exists, so nothing
  scoped per call is visible to it.
- `PeerRequestOptions.meta`. Rejected: the hand-written wire serializer
  extracts only the `Meta` extension; a header override there would still
  need a second channel to become an HTTP header, and nothing reads `meta`
  for that purpose.
- An ephemeral per-call MCP client under the approver's headers. Rejected:
  roughly 4 extra round trips per gated call, and it loses manager-driven
  cancellation and `tool_start`/progress events.
- A sidecar proxy rewriting headers in flight. Rejected: the proxy's own
  URL is frozen the same way the client's headers are today, so it
  degenerates into a persistent, body-inspecting proxy.
- **The `rmcp` request-extension side-channel: chosen.** `rmcp::model::Request`
  carries an http-crate-style typed `Extensions` map that rides the request
  value intact through `Peer::send_request_with_option`, the transport
  channel, and the worker into `post_message`, which receives the message
  by value.

## Decision Outcome

Chosen option: **the rmcp request-extension side-channel, webhook route
only.**

A `#[derive(Clone)] ApproverHeaders(HeaderMap)` (redacting `Debug`) rides
the one gated call's request value as an extension, inserted at every
`call_tool*` construction site and read back at the two send points that
own an outbound POST. One-call scoping is structural: nothing is keyed or
cleaned up, and concurrent calls never leak into each other.

Only the webhook route captures, and only for approvals raised by the
config gate (`ApprovalOrigin::ConfigGate`), never the agent-callable
`request_approval` surface ([#306](https://github.com/mezmo/aura/issues/306)).
The conversational route is excluded just as deliberately: its decision
caller is the session holder already on the stream.

Capture and delivery both fail closed. A missing mapped header errors the
call, naming every missing header and never a value. A gated stdio call
carrying an override is refused before dispatch. Cleartext capture stays
allowed but never silent: startup logs one warning per `[hitl]` config at
the HMAC boot-time seam, while an HMAC secret over `http://` remains a
boot-time misconfiguration, unchanged by capture. Audit is names-only: the
applied header names land on the `mcp.tool_call` span as `applied_headers`,
and a capture failure's error text is the event-level signal.

Mechanism detail (config shape, capture contract, send points, extension
lifecycle, audit fields): [docs/design/hitl.md](../design/hitl.md),
approver header forwarding section.

### Positive Consequences

- No `rmcp` fork and no version bump: the pinned client already carries the
  extension mechanism, unused until this feature reads it.
- One-call scoping needs no bookkeeping, because it is structural to how
  the extension rides the request value.
- HTTP-streamable and SSE share one insertion pattern and one read pattern;
  a third HTTP-shaped transport only needs the same read hook at its own
  send point.
- Audit stays cheap: a span attribute and an existing error path, no new
  wire type.

### Negative Consequences

- Stdio cannot deliver a per-call header at all, so a gated stdio call that
  carries an override fails closed. An operator who gates a stdio tool and
  also configures forwarding gets a hard failure, not a partial success.
- The read point sits inside a trait method whose signature `rmcp` owns; an
  upgrade past the pinned version (upstream 0.16.0 changes
  `post_message`'s shape) requires re-verifying the extension is still
  readable there before the bump lands.
- Cleartext capture is allowed with only a boot-time log warning, not a
  hard rejection. An operator who does not watch boot logs can run a
  credential-forwarding route over plaintext without noticing.
- Conversational-route forwarding does not exist. A deployment whose
  approver is the attended operator on the stream has no forwarding path
  today; extending one would need this design's premise re-examined.
- `request_approval` ([#306](https://github.com/mezmo/aura/issues/306))
  never forwards identity either, so the two approval surfaces are not
  symmetric.

## Links

- Design and implementation note: [docs/design/hitl.md](../design/hitl.md)
  (approver header forwarding section)
- Extends [2026-06-16-hitl-approval-architecture](2026-06-16-hitl-approval-architecture.md)
- Implements [#496](https://github.com/mezmo/aura/issues/496)
- Leaves open [#306](https://github.com/mezmo/aura/issues/306)
- RFC 2119: <https://www.rfc-editor.org/rfc/rfc2119>
