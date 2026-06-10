# ADR: Human-in-the-Loop (HITL) Architecture

**Status:** V1 decisions decided. V2+ decisions proposed/open.
**Date:** 2026-06-03
**Authors:** Mike Shearer
**Related:** LOG-23108, LOG-23462, LOG-23746

## Context

Aura's SRE agent cannot be dogfooded for incident remediation because there
is no mechanism to require human approval before destructive tool calls.
The agent runs autonomously end-to-end or not at all.

Three HITL mechanisms were identified during the sprint planning deep dive
(2026-05-27). This ADR records the architecture decisions for all three,
starting with V1 (Phase 1).

## Decision: Three HITL Surfaces

### Surface 1: Config-Driven Gate (V1 — decided)

Transparent interception at the `ToolWrapper` level. The operator configures
glob patterns in TOML; matching tool calls are intercepted before execution.
The LLM never sees the gate — it receives a rejection error if the webhook
denies the call.

**Insertion point:** `WrappedTool::call()` via new `pre_call` async hook on
`ToolWrapper` trait (additive, no breaking change). Composed first in the
`ComposedWrapper` chain so it fires before duplicate detection or scratchpad.

**Works in:** Single-agent and orchestration mode.

### Surface 2: Agent-Callable Tool (V1 — decided)

A `request_approval` Rig tool that any agent can call when it judges a
situation needs human input. Available to single-agent, coordinator, and
workers when `[hitl]` is configured.

**Pattern:** Regular Rig tool (like `submit_result` or `get_conversation_context`),
not a routing tool or MCP tool.

**Works in:** Single-agent and orchestration mode.

### Surface 3: Orchestration-Aware Tools (V2 — proposed)

Specialized versions of the callable tool with orchestration-specific behavior:

- **Coordinator routing tool** (`request_plan_approval`): first-write-wins
  routing decision, same pattern as `request_clarification`. Parks orchestration
  in interactive mode, calls webhook in headless mode.
- **Worker escalation tool**: worker calls `request_approval`, escalates to
  coordinator via `WorkerEvent::RequestsApproval` (LOG-23746 generalization).
  Coordinator decides whether to surface interactively or via webhook.

**Blocked on:** This ADR being reviewed and merged. LOG-23746 implementation.

## Decision: Webhook Contract

### Envelope (V1 — decided)

```json
{
  "version": 1,
  "request_type": "tool_gate",
  "request_id": "uuid-v4",
  "timestamp": "ISO-8601",
  "agent": {
    "name": "Aura Orchestrator",
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

### Type discriminator (V1 — decided)

- `tool_gate` — config-driven interception (Surface 1)
- `approval_request` — agent-called (Surface 2)

Future types (V2):
- `plan_review` — coordinator judges plan is risky (Surface 3)

### Items array (V1 — decided, V2 batching proposed)

V1 always sends a single-item array. The array schema enables V2 batch
approval via a centralized dispatcher that rolls up concurrent worker
requests into one webhook call per wave.

### Response (V1 — decided)

Per-item decisions:
```json
{
  "decisions": [
    { "approved": true },
    { "approved": false, "reason": "Outside maintenance window" }
  ]
}
```

Simplified single-decision format also accepted:
```json
{ "approved": true }
```

The wrapper accepts both shapes for backward compatibility.

## Decision: Approval Dispatch Trait (V1 — decided)

```rust
#[async_trait]
pub trait ApprovalDispatch: Send + Sync {
    async fn request_approval(&self, request: &ApprovalRequest)
        -> Result<ApprovalResponse, ApprovalError>;
}
```

V1 implements `HttpApprovalDispatch` only. The trait enables future dispatch
modes:
- `InteractiveApprovalDispatch` — terminal prompt for standalone CLI mode
- `BatchApprovalDispatch` — centralized dispatcher for parallel workers

## Decision: Sync-Block with Timeout (V1 — decided)

V1 webhook calls block with a configurable timeout (`hitl.timeout_secs`,
default 30). All error paths (timeout, HTTP error, parse failure) fail
closed — the tool call is rejected.

## Open Questions (V2+)

### Parking/Resume Architecture
- How does the orchestrator checkpoint mid-execution when a webhook returns
  `{"pending": true}`?
- What is the format of `hitl-pending.json`?
- How does the coordinator conversation reconstruct from artifacts on resume?
- How does the DAG represent `PendingApproval` as a task state?

### Parallel Worker Parking
- Multiple workers parked simultaneously: array checkpoint, partial approval,
  wave-level coordination
- Worker-level vs tool-level parking (blocking the agent loop vs blocking a
  single tool call)
- Centralized dispatcher: debounce window, batching, response routing

### A2A Integration
- `TaskState::InputRequired` is available in `a2a-lf` but not wired
- How does the A2A task update callback trigger orchestration resume?

### CLI Standalone Mode
- `webhook_url = "interactive"` as special value for terminal-prompt dispatch
- How does the CLI's `InteractiveApprovalDispatch` handle approval during
  the agent loop without deadlocking?

### Timeout Interaction
- `hitl.timeout_secs` must be less than `per_call_timeout_secs` in
  orchestration mode. Config validation emits a warning.
- Should HITL pause the per-call timeout (extend the budget)?

### Confidence-Based Auto-Approve
- Tony's LOG-23108 research: 70-90% execute with audit, 50-70% flag, <50%
  mandatory. Workers already report confidence via `submit_result`.
- How does confidence interact with config-driven gates (always mandatory)
  vs agent-callable tools (judgment-based)?

## Consequences

- V1 unblocks SRE agent dogfooding with config-driven approval gates
- The webhook contract is forward-compatible for V2 batching and V3
  orchestration-aware tools
- The `ApprovalDispatch` trait enables CLI standalone mode without
  changing wrapper or tool code
- V2+ work is blocked on this ADR being reviewed and merged
