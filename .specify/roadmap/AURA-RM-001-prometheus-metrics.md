# AURA-RM-001: Prometheus Metrics Endpoint

**Priority:** P0 (Critical)
**Status:** Implemented
**Dependencies:** AURA-RM-008 (Structured Error Taxonomy)
**Depended on by:** AURA-RM-003, AURA-RM-005, AURA-RM-011
**Affected Crates:** `aura-web-server`, `aura`
**Complexity:** Medium

## Rationale

Aura is deployed in production but has zero metrics exposure. Operators cannot set SLO alerts, measure request latency distributions, track token spend, or observe MCP tool health. The OpenTelemetry tracing infrastructure exists (`logging.rs`, `openinference_exporter.rs`) but covers traces only — not metrics. Without a `/metrics` endpoint, production operation is blind.

## User Stories

### US-001.1: Request Latency Observability

**As an** SRE,
**I want** to scrape a `/metrics` endpoint from Prometheus,
**So that** I can alert on p99 request latency exceeding SLO thresholds.

#### Acceptance Criteria

**AC-001.1.1:** Prometheus endpoint exists
- **Given** a running Aura web server
- **When** I send `GET /metrics`
- **Then** I receive a 200 response with Prometheus text exposition format

**AC-001.1.2:** Request latency histogram
- **Given** a chat completion request completes
- **When** I scrape `/metrics`
- **Then** I see a `aura_request_duration_seconds` histogram with `method`, `status`, and `agent` labels

### US-001.2: Token Usage Tracking

**As a** platform operator,
**I want** token usage counters per agent per LLM provider,
**So that** I can forecast cost and detect runaway usage.

#### Acceptance Criteria

**AC-001.2.1:** Token counters
- **Given** chat completion requests have been served
- **When** I scrape `/metrics`
- **Then** I see `aura_tokens_total` counter with `type` (prompt/completion), `provider`, and `agent` labels

### US-001.3: MCP Tool Duration

**As an** SRE,
**I want** MCP tool call duration histograms with server and tool name labels,
**So that** I can identify which tools are degrading response times.

#### Acceptance Criteria

**AC-001.3.1:** Tool duration histogram
- **Given** an agent has called MCP tools
- **When** I scrape `/metrics`
- **Then** I see `aura_mcp_tool_duration_seconds` histogram with `server`, `tool`, and `status` labels

### US-001.4: Error Rate by Type

**As an** SRE,
**I want** error rate counters by error type,
**So that** I can set differentiated alerts (LLM timeout vs MCP failure vs validation error).

#### Acceptance Criteria

**AC-001.4.1:** Error counters
- **Given** errors have occurred during request processing
- **When** I scrape `/metrics`
- **Then** I see `aura_errors_total` counter with `error_type` label using the taxonomy from AURA-RM-008

### US-001.5: Active Request Gauge

**As an** operator,
**I want** active in-flight request count as a gauge,
**So that** I can right-size capacity and detect connection leaks.

#### Acceptance Criteria

**AC-001.5.1:** In-flight gauge
- **Given** requests are being processed
- **When** I scrape `/metrics`
- **Then** I see `aura_requests_in_flight` gauge reflecting the current count

## Existing Infrastructure

- `ActiveRequestTracker` in `types.rs` already tracks in-flight count via `AtomicUsize`
- `UsageState` in `streaming_request_hook.rs` accumulates token counts per request
- MCP tool execution in `mcp_tool_execution.rs` has OTel span instrumentation with timing
- Error types will be defined by AURA-RM-008

## Edge Cases

- Metrics endpoint must not require authentication (Article VII of constitution)
- Histogram buckets must be configurable or use sensible defaults for LLM latency (seconds, not milliseconds)
- Token counters must handle streaming vs non-streaming responses identically
