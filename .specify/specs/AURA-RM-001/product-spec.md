# Product Spec: AURA-RM-001 Prometheus Metrics Endpoint

**Status:** Draft
**Roadmap Item:** AURA-RM-001
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Problem Statement

Aura has zero metrics exposure. Operators cannot set SLO alerts, measure request latency, track token spend, or observe MCP tool health. The only observability is OTel tracing, which requires a trace backend to query and doesn't support real-time alerting dashboards. Without a `/metrics` endpoint, production operation is blind.

## Scope

### In Scope
- Prometheus-compatible `/metrics` HTTP endpoint (text exposition format)
- Request latency histogram (by method, status, agent)
- Token usage counters (by type, provider, agent)
- MCP tool call duration histogram (by server, tool, status)
- Error counters (by error type from AURA-RM-008 taxonomy)
- In-flight request gauge

### Out of Scope
- Grafana dashboards or alerting rules (operator responsibility)
- Push-based metrics (only pull/scrape)
- Custom business metrics
- Per-user/tenant metrics (requires AURA-RM multi-tenancy)

## User Stories

### US-001.1: Request Latency Observability

**As an** SRE,
**I want** to scrape a `/metrics` endpoint from Prometheus,
**So that** I can alert on p99 request latency exceeding SLO thresholds.

**Priority:** P1

#### Acceptance Criteria

**AC-001.1.1:** Prometheus endpoint exists
- **Given** a running Aura web server
- **When** I send `GET /metrics`
- **Then** I receive a 200 response with `text/plain; version=0.0.4; charset=utf-8` content type

**AC-001.1.2:** Request latency histogram
- **Given** chat completion requests have been served
- **When** I scrape `/metrics`
- **Then** I see `aura_http_request_duration_seconds` histogram with `method`, `status_code`, and `agent` labels
- **And** the histogram has buckets appropriate for LLM latency (0.1, 0.5, 1, 2, 5, 10, 30, 60, 120, 300 seconds)

**AC-001.1.3:** Metrics endpoint does not require auth
- **Given** API auth is configured (AURA-RM-004, future)
- **When** I send `GET /metrics` without auth
- **Then** I receive 200 (not 401)

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
  - `provider`: `openai`, `anthropic`, `bedrock`, `gemini`, `ollama`
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
  - `tool`: tool name
  - `status`: `ok` or `error`

### US-001.4: Error Rate by Type

**As an** SRE,
**I want** error rate counters by error type,
**So that** I can set differentiated alerts.

**Priority:** P1

#### Acceptance Criteria

**AC-001.4.1:** Error counters
- **Given** errors have occurred during request processing
- **When** I scrape `/metrics`
- **Then** I see `aura_errors_total` counter with `error_type` label using the taxonomy from AURA-RM-008 (e.g., `llm_timeout`, `mcp_connection_failed`)

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

## API Contract

```
GET /metrics HTTP/1.1
Host: localhost:8080

HTTP/1.1 200 OK
Content-Type: text/plain; version=0.0.4; charset=utf-8

# HELP aura_http_request_duration_seconds Request latency histogram
# TYPE aura_http_request_duration_seconds histogram
aura_http_request_duration_seconds_bucket{method="POST",status_code="200",agent="SRE Assistant",le="0.5"} 12
...
# HELP aura_llm_tokens_total Token usage counter
# TYPE aura_llm_tokens_total counter
aura_llm_tokens_total{type="prompt",provider="bedrock",agent="SRE Assistant"} 45230
...
# HELP aura_mcp_tool_duration_seconds MCP tool call latency
# TYPE aura_mcp_tool_duration_seconds histogram
aura_mcp_tool_duration_seconds_bucket{server="pagerduty",tool="list_incidents",status="ok",le="1.0"} 8
...
# HELP aura_errors_total Error counter by type
# TYPE aura_errors_total counter
aura_errors_total{error_type="mcp_tool_error"} 3
...
# HELP aura_http_requests_in_flight In-flight request gauge
# TYPE aura_http_requests_in_flight gauge
aura_http_requests_in_flight 2
```

## Dependencies

- **Depends on:** AURA-RM-008 (error_type labels come from the taxonomy)
- **Depended on by:** AURA-RM-003 (circuit breaker metrics), AURA-RM-005 (budget metrics), AURA-RM-011 (admin endpoint)

## Success Criteria

- Prometheus can scrape `/metrics` and display all 5 metric families
- SLO alerts can be configured on request latency p99
- Token cost can be calculated from counter values
- MCP tool degradation is visible in tool duration histograms
