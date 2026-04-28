# AURA-RM-005: Token Budget Enforcement

**Priority:** P1 (High)
**Status:** Not Started
**Dependencies:** AURA-RM-001 (Metrics for budget tracking)
**Depended on by:** None
**Affected Crates:** `aura`, `aura-config`
**Complexity:** Low-Medium

## Rationale

The `turn_depth` config limits tool call rounds but not total token consumption. A model making 5 tool calls with massive context can burn unlimited tokens. The `UsageState` already tracks token counts per request but has no enforcement. Orchestration mode with replan cycles makes this especially risky.

## User Stories

### US-005.1: Per-Request Token Budget

**As a** platform operator,
**I want** to set a per-request token budget in TOML,
**So that** runaway loops are terminated before exceeding cost thresholds.

#### Acceptance Criteria

**AC-005.1.1:** Budget enforcement
- **Given** `token_budget = 10000` is configured
- **When** a request's cumulative token usage exceeds 10000
- **Then** the stream is terminated gracefully with an error message explaining the budget was exceeded

**AC-005.1.2:** Budget metrics
- **Given** metrics endpoint exists (AURA-RM-001)
- **When** a request is terminated due to budget
- **Then** `aura_budget_exceeded_total` counter is incremented

### US-005.2: Graceful Termination

**As an** operator,
**I want** the system to emit an error event and terminate the stream gracefully when budget is exceeded,
**So that** the client gets a clear explanation.

#### Acceptance Criteria

**AC-005.2.1:** SSE event on budget exceeded
- **Given** custom events are enabled
- **When** the token budget is exceeded mid-stream
- **Then** an `aura.budget_exceeded` event is emitted with token counts before the stream terminates with `[DONE]`

### US-005.3: Budget Audit Logging

**As an** operator,
**I want** budget violations logged with request details and token counts,
**So that** I can audit which queries are expensive.

#### Acceptance Criteria

**AC-005.3.1:** Structured log on violation
- **Given** a request exceeds its token budget
- **When** the stream is terminated
- **Then** a structured log entry is emitted with: request_id, agent_name, budget, actual_tokens, query_preview

## Configuration Example

```toml
[agent]
name = "SRE Assistant"
token_budget = 50000
# Optional: separate prompt/completion budgets
# prompt_token_budget = 30000
# completion_token_budget = 20000
```
