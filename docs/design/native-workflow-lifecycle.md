# Native configured workflow lifecycle

Configured `orchestration.stages` replace coordinator planning for that agent.
Existing agents with no stages retain model-directed orchestration. Stages run
in order, using named workers, the shared MCP manager, existing tool wrappers,
and the existing HITL decision store. No external workflow engine is required
for this single-instance implementation.

## Configuration and handoffs

See the [complete example](../../examples/complete/native-workflow.toml). Each
stage uses a named table such as `[orchestration.stages.act]` and specifies its
worker, operation, input bindings and output JSON Schema. The table name is the
stage ID; there is no separate `id` field. Set `orchestration.stage_order` to list
every stage ID exactly once in execution order. Table placement and alphabetical
key ordering never determine execution order. JSON pointers resolve
against `/input`, `/stages/<earlier_id>` and `/run`. Missing bindings fail before
that stage executes. Tool arguments and bindings cannot overwrite one another.
Schemas accept local fragment references only; schema loading does not fetch
files or remote resources.

Worker stages expose the configured result schema in `submit_result`. Invalid
submissions return correction feedback and do not occupy the result slot.
Unstructured fallback text cannot advance a configured workflow. Existing
unconfigured workers retain their string result contract.

Tool stages dispatch exact arguments through the registered worker tool server,
including its MCP filter, approval, observation and persistence wrappers.
Scratchpad substitution and turn-nudge prose are disabled for exact calls so
handoff validation receives the complete result. Configured workers do not get
the legacy `wait_for` tool, whose probe dispatcher bypasses worker wrappers.
Use verification stages for polling.

Verification requires an explicit `read_only = true` declaration. Its success
schema, optional failure schema, polling interval and wall-clock deadline are
configuration. A successful mutation is not evidence of recovery. A timeout
without matching evidence is `inconclusive`; it never becomes success. Polling
and waits have a maximum configured horizon of one year.

## Persistence and recovery

Set `memory_dir` on persistent storage. Workflow records live in
`memory_dir/workflows/<agent-and-session-digest>/<run-id>.json`. Run ownership is
an OS file lock. Writes use same-directory rename with file and directory sync.
Blocking writes retain their ownership lock through cancellation, and stale
queued writes cannot overwrite a newer state. Configured workflows skip legacy
artifact pruning so pending continuations keep their evidence. Operators must
retain or remove completed run directories according to their retention policy.

The record owns stage position, full handoffs, raw tool receipts, notification
progress, absolute deadlines, message digests and a sequenced event history.
Approval decisions remain authoritative in the existing approval store. Parked
worker conversations embed the existing park/reify checkpoint shape. Resume
uses its exact-call/scope checks and existing worker continuation.

Before dispatch, the record becomes `running`. Receipt persistence precedes
parsing and handoff validation. Restart after a recorded receipt consumes that
receipt; restart with no receipt becomes `uncertain` and cannot automatically
repeat the invocation. An interrupted notification is also uncertain. This is
not an exactly-once guarantee for a remote system. Reconcile uncertain remote
outcomes externally before starting any replacement run.

An active run checks parked decisions once per second without model calls.
Notifications/reminders use `on_approval_wait` offsets and ordinary MCP calls;
recipient lookup and channel delivery belong to those tools. Notification tools
cannot themselves require approval. Verification and timer deadlines survive
restarts.

Use `AURA_SESSION_STORE=file` and `AURA_SESSION_STORE_PATH` for durable A2A tasks
and approvals. The file backend permits one server writer. After process
restart, interrupted A2A tasks become `input-required`. Send a resume control in
the same agent/session with fresh request credentials. Resume continues safe
waits and recorded receipts, and rejects uncertain calls or changed execution
configuration. Credentials are not copied into workflow records. This version
does not automatically reconstruct authenticated clients at server startup,
or coordinate execution across multiple instances.

## A2A control contract

Start with a normal A2A text message; its message ID determines the run ID within
the agent/session. The first durable workflow snapshot exposes the run ID.
Alternatively, include a data part with an explicit run ID:

```json
{"data":{"aura.workflow":{"run_id":"59fe2bc4-9967-49ae-80a3-fcd562532b14","command":"start"}}}
```

Commands are `start`, `inspect`, `resume`, `takeover`, and `cancel`. Supply a new
A2A message ID for each operation. Repeating the same operation/message returns
the recorded state; reusing its ID with different content is rejected. Controls
are parsed before model input and never interpreted from natural language.

`takeover` pauses automatic execution and retains its suspended state, including
any approval checkpoint. `resume` explicitly returns control to Aura and checks
the approval store again. Takeover does not revoke an already recorded human
decision. If takeover interrupted an invocation, uncertainty prevents execution
from resuming. Cancellation preserves uncertainty even across repeated controls
and revokes pending approvals. A2A task cancellation applies this durable control
even after a server restart.

Controls use the same access boundary as existing A2A task operations. Agent and
session names scope records; they are not an authentication system. Deploy behind
the existing authenticated trusted-client boundary. Approval answers continue
through `/v1/approvals/{decision_id}`; an `approvalOutcome` in model output grants
no permission.

Durable snapshots stream as `aura.orchestrator.workflow_updated` and as replacing
A2A `workflow` artifacts. The A2A file backend retains the latest artifact and
history through restart. Snapshot event sequences allow clients to reconcile
missed updates; `inspect` returns the full record. Completed, failed and
inconclusive outcomes are distinct from input-required human/uncertain states.

A final configured MCP tool can export `/run` to a memory service. It receives
all preceding results, receipts and events; its own eventual receipt remains in
the local authoritative record. Memory retrieval and learned precedents remain
ordinary worker MCP/RAG capabilities, not execution authorization.

## Local verification

Run `cargo test --workspace` for schema, handoff, persistence and control tests.
`cargo test -p aura-web-server --test native_workflow_restart` launches real Aura
processes with a local MCP fixture. It checks approval-gated execution,
notification delivery, recovery verification, restart/resume and cancellation
after restart without provider credentials.
