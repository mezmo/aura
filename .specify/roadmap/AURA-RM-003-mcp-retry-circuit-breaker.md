# AURA-RM-003: MCP Retry and Circuit Breaker

**Priority:** P1 (High)
**Status:** Not Started
**Dependencies:** AURA-RM-001 (Metrics), AURA-RM-008 (Error Taxonomy)
**Depended on by:** AURA-RM-011 (Admin Endpoint)
**Affected Crates:** `aura`, `aura-config`
**Complexity:** Medium

## Rationale

MCP tool calls have zero retry logic or circuit breaking. If an MCP server is flaky (intermittent timeouts, 503s), every tool call fails through to the LLM. One dead MCP server causes cascading timeouts across all agent requests that use it. The execution path in `mcp_tool_execution.rs` and `mcp_streamable_http.rs` calls the MCP server exactly once and propagates any error.

## User Stories

### US-003.1: Automatic Retry on Transient Errors

**As an** agent operator,
**I want** MCP tool calls to automatically retry on transient errors with exponential backoff,
**So that** intermittent failures don't break agent workflows.

#### Acceptance Criteria

**AC-003.1.1:** Retry on transient errors
- **Given** an MCP server returns a 503 on the first attempt
- **When** the retry policy allows retries
- **Then** the tool call is retried up to the configured max attempts with exponential backoff

**AC-003.1.2:** No retry on permanent errors
- **Given** an MCP server returns a 400 (bad request)
- **When** the tool call fails
- **Then** the error is returned immediately without retry

### US-003.2: Circuit Breaker per MCP Server

**As an** operator,
**I want** a circuit breaker per MCP server that opens after N consecutive failures,
**So that** a dead MCP server returns fast errors instead of causing timeouts.

#### Acceptance Criteria

**AC-003.2.1:** Circuit breaker opens
- **Given** an MCP server has failed 5 consecutive times (configurable threshold)
- **When** a new tool call is attempted to that server
- **Then** the call fails immediately with a circuit-open error without contacting the server

**AC-003.2.2:** Circuit breaker half-open
- **Given** the circuit breaker is open and the reset timeout has elapsed
- **When** a tool call is attempted
- **Then** one probe request is sent; if it succeeds, the circuit closes; if it fails, it stays open

### US-003.3: Circuit Breaker Metrics

**As an** operator,
**I want** circuit breaker state exposed via the metrics endpoint,
**So that** I can alert on circuit breaker open events.

#### Acceptance Criteria

**AC-003.3.1:** Circuit breaker gauge
- **Given** metrics endpoint exists (AURA-RM-001)
- **When** I scrape `/metrics`
- **Then** I see `aura_mcp_circuit_breaker_state` gauge with `server` label (0=closed, 1=open, 0.5=half-open)

### US-003.4: Per-Server Configuration

**As a** config author,
**I want** to configure retry count, backoff, and circuit breaker thresholds per MCP server,
**So that** I can tune behavior for different tool reliability profiles.

#### Acceptance Criteria

**AC-003.4.1:** TOML configuration
- **Given** a config with retry and circuit breaker settings
- **When** the server starts
- **Then** the configured values are applied per MCP server
  ```toml
  [mcp.servers.example.retry]
  max_attempts = 3
  initial_backoff_ms = 100
  max_backoff_ms = 5000

  [mcp.servers.example.circuit_breaker]
  failure_threshold = 5
  reset_timeout_secs = 30
  ```

## Edge Cases

- Circuit breaker state should survive across requests but not across server restarts
- Retry backoff should include jitter to avoid thundering herd
- Retries must respect the overall request timeout (don't retry if timeout is nearly exhausted)
