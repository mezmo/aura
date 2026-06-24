# Human-in-the-loop approval gates

Human-in-the-loop (HITL) approval gates let an orchestration worker ask for
permission before running selected MCP tools. Use them for operations that need
a human decision before execution, such as production changes or destructive
actions.

Current behavior:

- Webhook routing works for unattended orchestration-worker approvals.
- Conversational routing works for attended orchestration-worker approvals over
  an open SSE stream. The AURA CLI in HTTP mode is the first attended client.
- Matching worker tool calls are blocked until the configured route approves
  them.
- Human denials are returned to the model as normal tool feedback, so the worker
  can explain the denial without treating it as a transport failure.
- Timeouts, cancellation, and webhook channel failures still fail closed as tool
  errors.
- Conversational HITL requires `stream=true`; non-streaming requests are
  rejected because approval prompts are delivered over SSE.
- Single-agent config gates are not composed in this phase. HITL gates are for
  orchestration workers.
- Approval lifecycle events emit on streaming responses. Webhook emits
  `aura.approval_requested` and `aura.approval_completed`; conversational also
  emits `aura.approval_pending` while the tool call is parked.

## Configure a webhook gate

Add a top-level `[hitl]` table and a required `[hitl.route]` table:

```toml
[hitl]
require_approval = ["kubectl_*", "restart_*", "dangerous_*"]

[hitl.route]
mode = "webhook"
url = "https://approvals.example.com/aura"
timeout_secs = 300
```

`require_approval` is a list of glob patterns matched against MCP tool names.
When an orchestration worker calls a matching tool, Aura posts an approval
request to the webhook before the MCP tool runs.

Patterns are matched in **config order, first-match wins**. The first glob that
matches a tool name triggers the approval request; later patterns are not
checked. A broad early pattern shadows a later, more specific one:

```toml
# k8s_* matches first for every k8s tool, including k8s_get_*.
# The k8s_get_* pattern never fires.
require_approval = ["k8s_*", "k8s_get_*"]

# Reorder so read-only tools are NOT gated. k8s_get_* matches first
# and short-circuits; k8s_* gates the rest.
require_approval = ["k8s_get_*", "k8s_*"]
```

This differs from the scratchpad subsystem, which uses longest-match (most
specific glob wins). HITL uses first-match because approval gating is a
security decision where explicit config ordering gives the operator direct
control over which pattern applies.

The `request_approval` tool is never matched by these globs. It is excluded from
the gate so the agent can ask for approval without triggering the gate itself.

`timeout_secs` defaults to `300` for webhooks. If the webhook does not return a
decision before the timeout, the tool does not run.

## Orchestration example

```toml
[agent]
name = "SRE Orchestrator"
system_prompt = "Route operational work to the right worker."
turn_depth = 8

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.2"
context_window = 200_000

[mcp.servers.k8s]
transport = "http_streamable"
url = "http://k8s-mcp:8080/mcp"

[hitl]
require_approval = ["k8s_apply_*", "restart_*", "delete_*"]

[hitl.route]
mode = "webhook"
url = "https://approvals.example.com/aura"
timeout_secs = 300

[orchestration]
enabled = true
max_planning_cycles = 2

[orchestration.worker.operations]
description = "Operational changes that may affect running services"
preamble = "Use Kubernetes tools carefully. Do not retry denied actions."
mcp_filter = ["k8s_*", "restart_*"]
```

The gate is added before the worker's MCP tools execute. A denied call returns a
successful blocked tool result to the worker:

```text
Tool call blocked by human approval denial: maintenance window not open. Do not execute this action.
```

The worker sees that message and can explain the denial to the user. The MCP tool
itself is not called.

## Webhook request

Aura sends a JSON request to the configured webhook URL. The request uses a flat
wire shape with `kind` tags for `scope` and `origin`:

```json
{
  "version": 1,
  "decision_id": "019edc27-e4d2-7950-abbf-e37a9060887d",
  "request_id": "req_d6df99fd5c8b4eb6af6e0de049e9c0d6",
  "scope": {
    "kind": "worker",
    "run_id": "019edc27-d7e1-73d2-ac33-1a1e21b3fffd",
    "task_id": 0,
    "worker": "operations",
    "session_id": "cs_264c5a09257c4089886cb00ae2ef03c4"
  },
  "origin": {
    "kind": "config_gate",
    "matched_pattern": "restart_*"
  },
  "items": [
    {
      "tool_name": "restart_deployment",
      "arguments": {
        "namespace": "prod",
        "deployment": "api"
      }
    }
  ]
}
```

Fields:

| Field | Meaning |
|-------|---------|
| `version` | Approval webhook protocol version. |
| `decision_id` | Unique id for this approval decision. |
| `request_id` | Aura request id for the chat completion. |
| `scope` | Agent surface asking for approval. Phase 1 emits worker scope for orchestration gates. |
| `origin` | Why approval was requested. Config gates use `kind = "config_gate"` with the matched glob. |
| `items` | Tool call payloads awaiting approval. Phase 1 sends one item per request. |

## Webhook response

Approve the tool call:

```json
{ "approved": true }
```

Deny the tool call, optionally with a reason:

```json
{ "approved": false, "reason": "maintenance window not open" }
```

Response behavior:

| Outcome | Tool execution | Worker-visible result |
|---------|----------------|-----------------------|
| `approved: true` | Runs the tool. | The worker receives the MCP tool result. |
| `approved: false` | Tool does not run. | The worker receives a blocked-action message with the denial reason. |
| Timeout | Tool does not run. | The worker receives a tool error: approval timed out. |
| Non-2xx response | Tool does not run. | The worker receives a tool error: approval channel error. |
| Invalid JSON response | Tool does not run. | The worker receives a tool error: approval channel error. |

## SSE lifecycle events

Approval routes emit lifecycle events on streaming responses. These events are
emitted even when `AURA_CUSTOM_EVENTS=false` because clients may need to react to
approval state.

```text
event: aura.approval_requested
data: { ... }

event: aura.approval_pending
data: { ... }

event: aura.approval_completed
data: { ... }
```

`aura.approval_requested` includes `decision_id`, `tool_name`, `origin`, and
`scope`. `aura.approval_pending` is emitted only by the conversational route and
contains the attended prompt payload that an Aura-aware client renders before
posting a decision. `aura.approval_completed` includes `decision_id`, terminal
`outcome`, `duration_ms`, and `scope`. Outcome kinds are `approved`, `denied`,
`timed_out`, `cancelled`, and `errored`; `errored` means the approval channel
failed before a human decision was obtained.

## Conversational route

Use conversational routing when the approver is present on the chat stream. The
server parks the worker tool call, sends `aura.approval_pending` over SSE, and
waits for a decision on the approval ingress endpoint:

```toml
[hitl]
require_approval = ["multiply", "divide", "dangerous_*"]

[hitl.route]
mode = "conversational"
timeout_secs = 120
```

The chat request must set `stream=true`. Aura rejects non-streaming requests for
conversational HITL because there is no channel for the pending approval prompt.

An attended client resolves a pending approval by POSTing the same decision shape
as a webhook response:

```http
POST /v1/approvals/{decision_id}
```

```json
{ "approved": false, "reason": "maintenance window not open" }
```

The AURA CLI supports this flow in HTTP mode. It renders
`aura.approval_pending`, prompts for approve/deny, and POSTs the decision back to
the server. One-shot CLI mode fails loud instead of prompting because it has no
interactive approval surface.

## Webhook manual smoke test

Start a webhook service that accepts the request shape above and returns an
approval response. Then run Aura with an orchestration config that uses:

```toml
[hitl]
require_approval = ["mock_tool"]

[hitl.route]
mode = "webhook"
url = "http://localhost:9988"
timeout_secs = 300
```

Use `mock_tool` (the tool name from `configs/example-math-orchestration.toml`)
so the glob matches a tool the worker actually calls. Put the webhook on a
different port than the mock MCP server (9999) to avoid a collision.

Ask an orchestration worker to use the gated tool. An approval should let the
tool run. A denial with a custom reason should produce a successful blocked tool
result containing that reason.

## Conversational manual smoke test

Run Aura with an orchestration config that uses `mode = "conversational"`, a
route timeout shorter than `[orchestration.timeouts].per_call_timeout_secs`, and
at least one gated worker tool. Connect with the AURA CLI in HTTP mode and send a
query that forces the worker to call the gated tool.

Expected behavior:

- The CLI renders an approval prompt from `aura.approval_pending`.
- Approving the prompt POSTs to `/v1/approvals/{decision_id}` and lets the tool
  run.
- Denying the prompt POSTs the denial and returns blocked-action feedback to the
  worker.

## Current limitations

- The webhook route is synchronous. Aura waits for the webhook response during
  the tool call.
- Conversational approvals are in-process only. The parked approval must be
  resolved by the same server process that emitted `aura.approval_pending`.
- Durable approval parking and cross-pod resume are not implemented yet.
- Webhook egress has no built-in authentication layer. Put auth, signing, or
  network controls in front of the webhook service.
