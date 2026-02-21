# Orchestration Prompt Templates

This document describes the template variables available for each orchestration prompt.

## Variable Syntax

Templates use `%%VARIABLE_NAME%%` for substitution. This syntax avoids conflicts with:
- JSON literals (`{` and `}`)
- Rust format strings (`{var}`)

## Editing Guidelines

1. **Preserve variables**: Ensure all `%%VAR%%` placeholders remain in the template
2. **Test changes**: Run `cargo test -p aura templates::` to verify templates match their context structs
3. **JSON examples**: Use `{` and `}` freely - only `%%VAR%%` is interpreted
4. **Optional sections**: Empty optional variables result in no extra whitespace

---

## Planning Prompt (`planning_prompt.md`)

Used by the coordinator to decompose user queries into execution plans.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%QUERY%%` | Yes | The user's original query |
| `%%WORKER_SECTION%%` | No | Formatted worker descriptions, or empty if no workers configured |
| `%%REFLECTION_SECTION%%` | No | Previous iteration context for replanning (iterations 2+) |
| `%%ERROR_SECTION%%` | No | Previous JSON parsing error for retry attempts |
| `%%WORKER_FIELD%%` | No | JSON field template showing worker assignment syntax |
| `%%WORKER_GUIDELINES%%` | No | Guidelines for worker assignment |

**Context struct**: `PlanningVars` in `templates.rs`

---

## Worker Task Prompt (`worker_task_prompt.md`)

Sent to worker agents when executing individual tasks.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%YOUR_TASK%%` | Yes | The specific task description to execute |
| `%%CONTEXT%%` | No | Prior completed task results (structured dependency values) |
| `%%ORCHESTRATION_GOAL%%` | Yes | The overall plan goal (context only — demoted to end) |

**Context struct**: `WorkerTaskVars` in `templates.rs`

---

## Synthesis Prompt (`synthesis_prompt.md`)

Used to combine results from multiple tasks into a coherent response.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%GOAL%%` | Yes | The orchestration goal |
| `%%QUERY%%` | Yes | The user's original query |
| `%%RESULTS%%` | Yes | Formatted task results (markdown sections) |

**Context struct**: `SynthesisVars` in `templates.rs`

---

## Evaluation Prompt (`evaluation_prompt.md`)

Used for semantic quality assessment of synthesized responses.

| Variable | Required | Description |
|----------|----------|-------------|
| `%%QUERY%%` | Yes | The user's original query |
| `%%GOAL%%` | Yes | The orchestration goal |
| `%%WORKERS_CONTEXT%%` | No | System context about available workers (for fact-checking) |
| `%%RESULT%%` | Yes | The synthesized response to evaluate |

**Context struct**: `EvaluationVars` in `templates.rs`

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
