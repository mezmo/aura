# Product Spec: AURA-RM-001 Prometheus Metrics Endpoint

**Status:** In Review (Remediation Round 1)
**Roadmap Item:** AURA-RM-001
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Problem Statement

Aura has zero metrics exposure. Operators cannot set SLO alerts, measure request latency, track token spend, or observe MCP tool health. The only observability is OTel tracing, which requires a trace backend and doesn't support real-time alerting dashboards. Without a `/metrics` endpoint, production operation is blind.

## Scope

### In Scope
- Prometheus-compatible `/metrics` HTTP endpoint (text exposition format)
- Request latency histogram (by method, status, agent)
- Token usage counters (by type, provider, agent)
- MCP tool call duration histogram (by server, tool, status)
- Error counters (by error type from AURA-RM-008 taxonomy)
- In-flight request gauge
- MCP server connection state gauge
- Access control: metrics bind to a configurable address (default localhost only)
- Kill switch: `AURA_METRICS_ENABLED` env var (default true)

### Out of Scope
- Grafana dashboards or alerting rules (operator responsibility)
- Push-based metrics (only pull/scrape)
- Per-user/tenant metrics
- Process-level metrics (CPU, memory, FDs — use node exporter or container metrics)

## User Stories

### US-001.1: Request Latency Observability

**As an** SRE,
**I want** to scrape a `/metrics` endpoint from Prometheus,
**So that** I can alert on p99 request latency exceeding SLO thresholds.

**Priority:** P1

#### Acceptance Criteria

**AC-001.1.1:** Prometheus endpoint exists
- **Given** a running Aura web server with `AURA_METRICS_ENABLED=true` (default)
- **When** I send `GET /metrics`
- **Then** I receive a 200 response with `text/plain; version=0.0.4; charset=utf-8` content type

**AC-001.1.2:** Request latency histogram
- **Given** chat completion requests have been served
- **When** I scrape `/metrics`
- **Then** I see `aura_http_request_duration_seconds` histogram with `method`, `status_code`, and `agent` labels
- **And** the histogram has buckets: `[0.025, 0.1, 0.5, 1, 2, 5, 10, 30, 60, 120, 300]` (includes sub-100ms for fast-fail visibility)

**AC-001.1.3:** Metrics disabled when kill switch is off
- **Given** `AURA_METRICS_ENABLED=false`
- **When** I send `GET /metrics`
- **Then** I receive 404

### US-001.2: Token Usage Tracking

**As a** platform operator,
**I want** token usage counters per agent per LLM provider,
**So that** I can forecast cost and detect runaway usage.

**Priority:** P1

#### Acceptance Criteria

**AC-001.2.1:** Token counters
- **Given** chat completion requests have been served
- **When** I scrape `/metrics`
- **Then** I see `aura_llm_tokens_total` counter with labels:
  - `type`: `prompt` or `completion`
  - `provider`: value from `Agent::get_provider_info()` (not hardcoded)
  - `agent`: agent name from config

### US-001.3: MCP Tool Duration

**As an** SRE,
**I want** MCP tool call duration histograms,
**So that** I can identify which tools are degrading response times.

**Priority:** P1

#### Acceptance Criteria

**AC-001.3.1:** Tool duration histogram
- **Given** an agent has called MCP tools
- **When** I scrape `/metrics`
- **Then** I see `aura_mcp_tool_duration_seconds` histogram with labels:
  - `server`: MCP server name from config
  - `tool`: tool name (bounded — see cardinality note below)
  - `status`: `ok` or `error`
- **And** buckets: `[0.01, 0.05, 0.1, 0.25, 0.5, 1, 2, 5, 10, 30, 60]` (includes 60s for long-running tools)

### US-001.4: Error Rate by Type

**As an** SRE,
**I want** error rate counters by error type,
**So that** I can set differentiated alerts.

**Priority:** P1

#### Acceptance Criteria

**AC-001.4.1:** Error counters
- **Given** errors have occurred during request processing
- **When** I scrape `/metrics`
- **Then** I see `aura_errors_total` counter with `error_type` label using the taxonomy from AURA-RM-008 (e.g., `llm_timeout`, `mcp_tool_error`)

### US-001.5: Active Request Gauge

**As an** operator,
**I want** active in-flight request count as a gauge,
**So that** I can right-size capacity.

**Priority:** P2

#### Acceptance Criteria

**AC-001.5.1:** In-flight gauge
- **Given** requests are being processed
- **When** I scrape `/metrics`
- **Then** I see `aura_http_requests_in_flight` gauge reflecting the current count

### US-001.6: MCP Server Connection State

**As an** operator,
**I want** MCP server connection state as a gauge,
**So that** I can alert when MCP servers disconnect.

**Priority:** P2

#### Acceptance Criteria

**AC-001.6.1:** MCP connection gauge
- **Given** MCP servers are configured
- **When** I scrape `/metrics`
- **Then** I see `aura_mcp_server_connected` gauge with `server` label (1 = connected, 0 = disconnected)

## Cardinality Constraints

Metric label values MUST be sourced from config-defined sets, never from user input:
- `agent`: from TOML `agent.name` — bounded by number of loaded configs
- `server`: from TOML `mcp.servers.<name>` — bounded by config
- `tool`: from MCP `tools/list` response — **potentially unbounded**. If a server exposes more than 100 unique tool names, tools beyond the first 100 are aggregated under the label `_other`.
- `provider`: from LLM config — bounded (5 providers: openai, anthropic, bedrock, gemini, ollama)
- `status_code`: from HTTP response — bounded (~5 codes: 200, 400, 401, 500, 503)
- `error_type`: from ErrorCategory — bounded (13 categories)

## Metric Label Sensitivity Note

Prometheus labels contain operational topology information (agent names, MCP server names, tool names, provider names). This data is considered sensitive. The `/metrics` endpoint must NOT be exposed to untrusted networks. Use the `AURA_METRICS_BIND_ADDRESS` config to restrict access (default: `127.0.0.1:9090`).

## Dependencies

- **Depends on:** AURA-RM-008 (error_type labels come from the taxonomy)
- **Depended on by:** AURA-RM-003 (circuit breaker metrics), AURA-RM-005 (budget metrics), AURA-RM-011 (admin endpoint)

## Success Criteria

- Prometheus can scrape `/metrics` and display all 6 metric families
- SLO alerts can be configured on request latency p99
- Token cost can be calculated from counter values
- MCP tool degradation is visible in tool duration histograms
- Metrics endpoint is not accessible from untrusted networks by default
