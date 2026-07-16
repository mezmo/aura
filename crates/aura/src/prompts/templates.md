# Orchestration Prompt Templates

This document describes the template variables available for each orchestration prompt.

## Variable Syntax

All templates use a single placeholder convention: `%%VARIABLE_NAME%%`. This syntax avoids conflicts with:
- JSON literals (`{` and `}`)
- Rust format strings (`{var}`)

## Editing Guidelines

1. **Preserve variables**: Ensure all `%%VAR%%` placeholders remain in the template
2. **Test changes**: Run `cargo test -p aura templates::` to verify templates match their context structs
3. **JSON examples**: Use `{` and `}` freely - only `%%VAR%%` is interpreted
4. **Optional sections**: Empty optional variables result in no extra whitespace

---

## Coordinator Preamble (`orchestrator_preamble.md`)

Layered system prompt for the coordinator agent.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%ORCHESTRATION_SYSTEM_PROMPT%%` | Yes | User-provided domain playbook |
| `%%TOOLS_SECTION%%` | Yes | Routing/artifact/history tool sentence |
| `%%RECON_GUIDANCE%%` | Yes | Reconnaissance guidance or worker/tool-name clarification |

**Context struct**: `CoordinatorPreambleVars` in `templates.rs`

---

## Worker Preamble (`worker_preamble.md`)

Fallback system prompt for a generic worker.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%WORKER_SYSTEM_PROMPT%%` | Yes | Custom or default worker instructions |

**Context struct**: `WorkerPreambleVars` in `templates.rs`

---

## Planning Prompt (`planning_prompt.md`)

Initial routing user message.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%TIMESTAMP%%` | Yes | Current UTC timestamp |
| `%%QUERY%%` | Yes | Verbatim user query |
| `%%WORKER_SECTION%%` | No | Worker roster section (empty when no workers configured) |
| `%%WORKER_GUIDELINES%%` | No | Worker-assignment guidelines (empty when no workers configured) |

**Context struct**: `PlanningVars` in `templates.rs`

---

## Worker Roster (`worker_roster.md`)

Worker roster section rendered inside the planning prompt.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%HEADER_NOTE%%` | No | Role-assignment note for Summary/Full visibility |
| `%%ROSTER_CONTENT%%` | Yes | Worker entries (format depends on `tools_in_planning`) |
| `%%CLOSING_LINE%%` | Yes | Closing instruction line |

**Context struct**: `WorkerRosterVars` in `templates.rs`

---

## Worker Guidelines (`worker_guidelines.md`)

Worker-assignment guidelines rendered inside the planning prompt.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%VALID_WORKER_NAMES%%` | Yes | Comma-separated quoted worker names |

**Context struct**: `WorkerGuidelinesVars` in `templates.rs`

---

## Continuation Prompt (`continuation_prompt.md`)

End-of-iteration decision-point user message.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%ITERATION%%` | Yes | Current iteration number |
| `%%MAX_ITERATIONS%%` | Yes | Planning-cycle budget |
| `%%URGENCY%%` | No | `(FINAL ATTEMPT)` on the last iteration |
| `%%SUCCEEDED%%` | Yes | Completed task count |
| `%%TOTAL%%` | Yes | Total task count |
| `%%GOAL%%` | Yes | Pinned original user query |
| `%%COMPLETED_SECTION%%` | No | Completed task entries |
| `%%BLOCKED_SECTION%%` | No | Blocked task entries |
| `%%REDESIGN_SECTION%%` | No | Failed task entries |
| `%%FAILURE_SECTION%%` | No | Failure summary and gaps |
| `%%FAILURE_HISTORY%%` | No | Accumulated failure history |
| `%%REUSE_GUIDANCE%%` | No | Result-forwarding guidance when relevant |

**Context struct**: `ContinuationVars` in `templates.rs`

---

## Continuation Wrapper (`continuation_wrapper.md`)

Timestamp prefix for the continuation user message.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%TIMESTAMP%%` | Yes | Current UTC timestamp |
| `%%CONTINUATION_BODY%%` | Yes | Rendered continuation prompt body |

**Context struct**: `ContinuationWrapperVars` in `templates.rs`

---

## Worker Task Prompt (`worker_task_prompt.md`)

Sent to worker agents when executing individual tasks.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%YOUR_TASK%%` | Yes | The specific task description to execute |
| `%%CONTEXT%%` | No | Prior completed task results (structured dependency values) |

**Context struct**: `WorkerTaskVars` in `templates.rs`

---

## Session History (`session_history.md`)

Prior-run context block appended to the coordinator preamble.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%TURN_ENTRIES%%` | Yes | Rendered turn entries |
| `%%TURN_COUNT%%` | Yes | Number of prior runs shown |

**Context struct**: `SessionHistoryVars` in `templates.rs`

---

## Duplicate-Call Guard (`duplicate_call_guidance.md`, `duplicate_call_abort.md`)

Annotations injected into worker tool outputs on the duplicate-loop path.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%TOOL_NAME%%` | Yes | Tool name repeated |
| `%%COUNT%%` | Yes | Consecutive identical-call count |

**Context struct**: `DuplicateCallVars` in `templates.rs`

---

## Type Safety

Templates are validated by tests in `templates.rs`:

1. **Bi-directional validation**: Tests verify that:
   - All `%%VAR%%` placeholders in templates are provided by their context struct
   - All fields in context structs are used in their template

2. **Run validation**: `cargo test -p aura templates::`

3. **If you add a variable**:
   - Add to both the template file AND the corresponding `*Vars` struct
   - Add to the struct's `VARS` constant
   - Update the struct's `render()` method
   - Run tests to verify

4. **If you remove a variable**:
   - Remove from both the template file AND the struct
   - Run tests to verify no orphaned references
