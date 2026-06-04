# Human-in-the-Loop (HITL) Integration Guide

Aura can gate tool execution behind an external approval service. When a
tool call matches a configured pattern, Aura sends a webhook to your
service and waits for approval before executing.

## Quick Start

Add a `[hitl]` section to your Aura TOML config:

```toml
[hitl]
enabled = true
webhook_url = "https://your-approval-service.example.com/approve"
timeout_secs = 30
require_approval = ["kubectl_delete_*", "scale_*", "restart_*"]
```

Aura sends a `POST` to your webhook URL whenever the LLM calls a tool
matching one of the `require_approval` patterns. Your service responds
with `{"approved": true}` or `{"approved": false, "reason": "..."}`.

## Two Approval Surfaces

### 1. Config-Driven Gate (automatic)

Tools matching `require_approval` glob patterns are intercepted
transparently. The LLM does not know about the gate — it receives a
rejection error if the webhook denies the call.

**Use for:** hard policy rules. "kubectl_delete always needs approval."

### 2. Agent-Callable Tool (explicit)

When `[hitl]` is enabled, agents also receive a `request_approval` tool
they can call explicitly. The agent decides when to ask based on its
system prompt and judgment.

**Use for:** soft rules. "Before destructive actions, ask for permission."

Both surfaces use the same webhook URL and response format.

## Webhook Contract

### Request

Aura sends `POST` with `Content-Type: application/json`.

```json
{
  "version": 1,
  "request_type": "tool_gate",
  "request_id": "9f0ba5a9-94e7-4d38-aacf-116f154470d6",
  "timestamp": "2026-06-03T19:52:50.702935+00:00",
  "agent": {
    "name": "Cluster Orchestrator",
    "run_id": "abc-123",
    "session_id": "sess-456"
  },
  "items": [
    {
      "tool_name": "kubectl_delete_pod",
      "arguments": { "namespace": "prod", "pod": "web-abc" },
      "matched_pattern": "kubectl_delete_*",
      "task_id": 3,
      "worker_name": "remediation"
    }
  ]
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `version` | integer | Protocol version. Always `1`. |
| `request_type` | string | `"tool_gate"` for config-driven interception, `"approval_request"` for agent-initiated. |
| `request_id` | string | UUID v4. Unique per approval request. Use for audit logging and future callback correlation. |
| `timestamp` | string | ISO 8601 timestamp of the request. |
| `agent.name` | string | Name of the Aura agent (from `[agent] name` in TOML). |
| `agent.run_id` | string, omitted when absent | Orchestration run ID. Present in orchestration mode, omitted in single-agent mode. |
| `agent.session_id` | string, omitted when absent | Session ID from the HTTP request. Omitted when not provided. |
| `items` | array | Tool calls awaiting approval. V1 always sends exactly one item. |
| `items[].tool_name` | string | The MCP tool being called (e.g. `kubectl_delete_pod`). For `request_type: "approval_request"`, this is `"request_approval"`. |
| `items[].arguments` | object | The arguments the LLM is passing to the tool. For `approval_request`, this contains `action_description`, `risk_rationale`, and optional `context`. |
| `items[].matched_pattern` | string, omitted when absent | The glob pattern that matched (e.g. `"kubectl_delete_*"`). Omitted for agent-initiated requests. |
| `items[].task_id` | integer, omitted when absent | Orchestration task ID. Omitted in single-agent mode. |
| `items[].worker_name` | string, omitted when absent | Orchestration worker name. Omitted in single-agent mode. |

### Response

Return HTTP 200 with `Content-Type: application/json`.

**Simple format:**

```json
{ "approved": true }
```

```json
{ "approved": false, "reason": "Outside maintenance window" }
```

**Batch format** (for future multi-item support):

```json
{
  "decisions": [
    { "approved": true },
    { "approved": false, "reason": "Too risky" }
  ]
}
```

Both formats are accepted. When `decisions` is present, it takes
precedence over the top-level `approved` field.

#### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| `approved` | boolean | Whether the tool call is approved. |
| `reason` | string (optional) | Explanation for rejection. Shown to the LLM, which relays it to the user. |
| `decisions` | array (optional) | Per-item decisions. V1 reads `decisions[0]`. |

### Error Behavior

Aura fails closed on all error conditions:

| Scenario | Behavior |
|----------|----------|
| Webhook returns `{"approved": true}` | Tool executes normally. |
| Webhook returns `{"approved": false, "reason": "..."}` | Tool rejected. LLM receives the reason and can explain to the user. |
| Webhook returns non-2xx status | Tool blocked. Error message includes the HTTP status. |
| Webhook does not respond within `timeout_secs` | Tool blocked. Error message mentions timeout. |
| Webhook unreachable (connection refused) | Tool blocked. Error message includes the connection error. |
| Response body is not valid JSON | Tool blocked. Error message includes parse error. |
| `approved` field missing and no `decisions` array | Treated as rejected (fail-closed). |

## Configuration Reference

```toml
[hitl]
# Master switch. When false, no approval gates are active and the
# request_approval tool is not registered.
enabled = true

# URL that receives approval webhook POSTs. Required when enabled.
webhook_url = "https://your-service.example.com/approve"

# Maximum seconds to wait for a webhook response. If the webhook does
# not respond within this window, the tool call is blocked.
# Default: 30
timeout_secs = 30

# Glob patterns for tool names that require automatic approval.
# Supports * (any characters) and ? (single character).
# Tools not matching any pattern execute without approval.
# Default: [] (no automatic gates; only the agent-callable tool is active)
require_approval = ["kubectl_delete_*", "scale_*", "restart_*"]
```

### Glob Pattern Examples

| Pattern | Matches | Does Not Match |
|---------|---------|----------------|
| `kubectl_delete_*` | `kubectl_delete_pod`, `kubectl_delete_deployment` | `kubectl_get_pods` |
| `scale_*` | `scale_deployment`, `scale_statefulset` | `describe_scale` |
| `*_write` | `db_write`, `config_write` | `db_read` |
| `dangerous_*` | `dangerous_delete`, `dangerous_restart` | `safe_delete` |
| `*` | everything | nothing (gates all tools) |

### Timeout Considerations

In orchestration mode, each worker has a per-call timeout
(`[orchestration.timeouts] per_call_timeout_secs`, default 120s). The
HITL `timeout_secs` must be less than the per-call timeout, or the
worker will be killed before the webhook responds.

In single-agent mode, there is no per-call timeout — the HITL timeout
is the only constraint.

## Agent-Initiated Approval

When `[hitl]` is enabled, agents receive a `request_approval` tool.
Guide the agent to use it via the system prompt:

```toml
[agent]
system_prompt = """
You have access to operations tools. Some are destructive.

APPROVAL RULES:
- Before calling any destructive tool (delete, drop, restart), call
  request_approval first. Describe the action and why it's needed.
- If approval is denied, explain the rejection to the user. Do not retry.
- Non-destructive tools (list, describe, query) do not need approval.
"""
```

The `request_approval` tool accepts:

| Argument | Type | Required | Description |
|----------|------|----------|-------------|
| `action_description` | string | yes | What the agent wants to do. |
| `risk_rationale` | string | yes | Why the agent thinks this needs approval. |
| `context` | object | no | Additional structured metadata for the reviewer. |

The webhook receives this as a `request_type: "approval_request"` with
the tool arguments in `items[0].arguments`.

## Example: Kubernetes SRE Agent

```toml
[hitl]
enabled = true
webhook_url = "https://approval.internal.example.com/api/v1/approve"
timeout_secs = 60
require_approval = [
  "scale_*", "restart_*", "patch_*",
  "delete_*", "rollback_*", "apply_*", "create_*"
]

[agent]
name = "Cluster Orchestrator"
system_prompt = """
You are a Kubernetes cluster operations coordinator.

Destructive operations (scale, restart, patch, delete, rollback, apply,
create) require human approval. The approval gate is automatic — you do
not need to request it explicitly. If approval is denied, explain the
rejection to the user and suggest alternatives.
"""
turn_depth = 8

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-5.2"
context_window = 200_000

[mcp]
sanitize_schemas = true

[mcp.servers.kubernetes]
transport = "http_streamable"
url = "http://kubernetes-mcp:8080/mcp"
description = "Kubernetes cluster tools"
```

## Implementing a Webhook Service

A minimal webhook receiver in Python:

```python
from http.server import HTTPServer, BaseHTTPRequestHandler
import json

class ApprovalHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(
            int(self.headers["Content-Length"])))

        tool = body["items"][0]["tool_name"]
        args = body["items"][0]["arguments"]

        # Your approval logic here
        approved = tool not in ["kubectl_delete_namespace"]

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        response = {"approved": approved}
        if not approved:
            response["reason"] = f"Auto-rejected: {tool} is prohibited"
        self.wfile.write(json.dumps(response).encode())

HTTPServer(("0.0.0.0", 8080), ApprovalHandler).serve_forever()
```

### Integration Patterns

| Pattern | Description |
|---------|-------------|
| **Auto-approve with audit** | Always return `approved: true`, log every request for compliance. |
| **Slack/Teams approval** | Post to a channel, wait for reaction, return decision. Set `timeout_secs` high enough. |
| **PagerDuty gate** | Only approve during active incidents. Check PD API before responding. |
| **Time-window policy** | Approve only during maintenance windows. Reject with next window time as reason. |
| **Role-based** | Use `agent.session_id` or forwarded headers to check the requesting user's permissions. |

## Testing Locally

A test webhook stub is provided at `aura-sandbox/hitl-test/webhook-stub.py`:

```bash
# Interactive mode — prompts for each request
python3 webhook-stub.py --port 9999 --mode prompt

# Auto-approve everything
python3 webhook-stub.py --port 9999 --mode approve

# Auto-reject with a reason
python3 webhook-stub.py --port 9999 --mode reject --reason "testing"

# Never respond (test timeout behavior)
python3 webhook-stub.py --port 9999 --mode timeout
```
