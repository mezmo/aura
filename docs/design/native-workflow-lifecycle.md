# Native configured workflow lifecycle

Use a configured workflow when you know which steps Aura should run and in what
order. An incident workflow might start with an investigation and a proposed
remediation. Aura can wait for approval before applying the change, then check
whether it worked.
Aura saves progress so a caller can resume the run after a restart.

The workflow uses Aura's existing workers, MCP tools, approval store, and
park/reify support for saving and resuming worker conversations. It runs on one
Aura instance and does not require an external workflow engine. Agents without
configured stages continue to use coordinator planning.

## Define the stages

The [complete example](../../examples/complete/native-workflow.toml) shows an
incident workflow. Define each stage in a named table such as
`[orchestration.stages.act]`. The table name is the stage ID. Set
`orchestration.stage_order` to list every stage ID exactly once in execution
order. Moving tables around in the file does not change that order.

Each stage specifies a worker, an operation, input bindings, and an output JSON
Schema. The operation determines what Aura does:

- `worker` asks a worker to perform the task and submit a result.
- `tool` calls an MCP tool with the configured arguments.
- `wait` pauses for the configured number of seconds.
- `verify` polls a tool until its response matches a success or failure schema,
  or the deadline expires.

Input bindings use JSON pointers. `/input` refers to the original input.
`/stages/<earlier_id>` refers to an earlier stage's result, and `/run` refers to
the run record.
A missing input stops the stage before it executes. A binding cannot overwrite
an argument already set in the configuration.

Aura validates each result before passing it to the next stage. For worker
stages, `submit_result` exposes the configured schema. If a worker submits an
invalid result, it gets feedback and can try again. Free-form text cannot
advance the workflow. Workers outside configured workflows keep their existing
string result format. Schemas can use local fragment references; Aura does not
load schema references from files or remote services.

Tool stages use the worker's MCP permissions and existing approval, observation,
and persistence wrappers. For exact tool calls, Aura disables scratchpad
substitution and turn-nudge text so validation receives the full tool response.
Configured workers use verification stages instead of the legacy `wait_for`
tool, which bypasses those wrappers.

## Wait for approval and check the result

Approval decisions come from the existing approval store. An active run checks
for a decision once per second without calling a model. Once approved, Aura
uses the existing park/reify checks to resume the approved call in its original
scope. A model returning `approvalOutcome` does not grant permission.

Use `on_approval_wait` to send notifications or reminders at configured offsets
while approval is pending. These are ordinary MCP calls. The MCP tool handles
recipient lookup and delivery, and must not require approval itself.

An action completing successfully does not prove that the problem is fixed.
A `verify` stage checks the response against a success schema and, optionally,
a failure schema. Set its polling interval and deadline, and declare
`read_only = true` for the probe. If the deadline expires without matching
evidence, the result is `inconclusive`. Waits and polling have a maximum
configured horizon of one year. Their deadlines survive a restart.

## Save progress and recover after a restart

Put `memory_dir` on persistent storage. Aura saves each run at
`memory_dir/workflows/<agent-and-session-digest>/<run-id>.json`. The record
contains the current stage, complete stage results, saved tool responses
(receipts), notification progress, deadlines, message digests, and event history.
Approval decisions stay in the approval store.

Aura marks an invocation `running` before dispatch and saves its receipt before
parsing or validating it. If Aura restarts after saving the receipt, resume uses
that receipt. If an invocation was in progress and no receipt was saved, the run
becomes `uncertain`. Aura will not automatically repeat that invocation. This
also applies to interrupted notification calls. Check the remote system's
outcome before starting a replacement run; Aura cannot guarantee exactly-once
execution in that system.

Set `AURA_SESSION_STORE=file` and `AURA_SESSION_STORE_PATH` to persist A2A tasks
and approvals. After a restart, interrupted A2A tasks become `input-required`.
Send a `resume` control in the same agent/session with fresh request credentials.
Aura continues saved waits and receipts, but refuses to execute uncertain calls
or resume with changed execution configuration. It does not automatically
rebuild authenticated clients at startup or coordinate multiple Aura instances.

An OS file lock gives one writer ownership of a run. Aura saves files with a
same-directory rename and syncs the file and directory. Blocking writes retain
the lock through cancellation; an older queued write cannot overwrite newer
state. The file session backend also permits only one server writer.

Configured workflows skip legacy artifact pruning so a paused worker keeps its
evidence. Apply your retention policy to completed run directories.

## Control a run through A2A

Start with a normal A2A text message. Its message ID determines the run ID within
the agent/session, and the first saved workflow snapshot exposes that ID. You
can also supply an explicit run ID in a data part:

```json
{"data":{"aura.workflow":{"run_id":"59fe2bc4-9967-49ae-80a3-fcd562532b14","command":"start"}}}
```

The commands are `start`, `inspect`, `resume`, `takeover`, and `cancel`. Use a new
A2A message ID for each operation. Repeating the same operation and message
returns the saved state. Reusing its message ID with different content is an
error. Aura reads controls before model input; natural-language requests do not
act as controls.

`takeover` pauses automatic execution and keeps the saved state, including any
approval checkpoint. It does not revoke an approval already recorded by a
human. `resume` returns control to Aura and checks the approval store again. If
takeover interrupted an invocation, Aura will not resume it. Check its outcome
externally before starting a replacement run.

`cancel` revokes pending approvals and preserves any uncertainty about calls
already dispatched. A2A task cancellation applies this control even after a
server restart.

Use the same authenticated, trusted-client boundary as other A2A task operations.
Agent and session names separate records; they do not authenticate the caller.
Submit approval answers through `/v1/approvals/{decision_id}`.

## Read or export the run record

Aura streams snapshots as `aura.orchestrator.workflow_updated` events and A2A
`workflow` artifacts. Each artifact replaces the previous snapshot. The file
backend keeps the latest artifact and task history through restart. Event
sequence numbers help clients detect missed updates; `inspect` returns the full
record. Completed, failed, and inconclusive runs have distinct outcomes. Runs
that need human input or have uncertain execution remain input-required.

A final MCP tool stage can export `/run` to a memory service. It receives all
preceding results, receipts, and events. The export call's own receipt stays in
the local run record. Workers can still retrieve prior knowledge through MCP
or RAG; that knowledge does not authorize execution.

## Local verification

Run `cargo test --workspace` for schema, handoff, persistence, and control tests.
`cargo test -p aura-web-server --test native_workflow_restart` launches real Aura
processes with a local MCP fixture. It checks execution after approval,
notification delivery, recovery verification, restart/resume, and cancellation
after restart without provider credentials.
