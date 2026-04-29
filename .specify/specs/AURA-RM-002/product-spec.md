# Product Spec: AURA-RM-002 Deep Health and Readiness Checks

**Status:** In Review
**Roadmap Item:** AURA-RM-002
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Problem Statement

The current `/health` endpoint returns a static `{"status": "healthy"}` regardless of actual system state. Kubernetes liveness/readiness probes never detect a degraded instance -- failed LLM credentials, disconnected MCP servers, or unreachable vector stores all present as "healthy." This causes traffic to route to broken pods, resulting in user-facing errors that could be prevented by removing unhealthy pods from the load balancer.

Operators currently have no way to diagnose which subsystem is failing without reading application logs. A structured health endpoint with per-subsystem status enables:
- Kubernetes readiness probes that remove broken pods
- Kubernetes liveness probes that restart stuck processes
- On-call diagnostics via a single HTTP call
- Prometheus alerting on subsystem health gauges

## Scope

### In Scope
- Readiness endpoint (`/health/ready`) verifying LLM, MCP, and vector store connectivity
- Liveness endpoint (`/health/live`) confirming process responsiveness
- Per-subsystem status breakdown in JSON response
- Cached health check results with configurable TTL
- Prometheus metrics for health check outcomes (`aura_health_ready` gauge, per-subsystem gauges, check duration histogram)
- CLI/env var configuration for cache TTL and probe timeout
- Sanitized error messages in health responses (no internal IPs, hostnames, or DNS names)

### Out of Scope
- Automated remediation or restart logic (Kubernetes handles this)
- Circuit breaker integration (AURA-RM-003 -- consumes health check status)
- Admin endpoint for manual health management (AURA-RM-011)
- Authentication on health endpoints (explicitly unauthenticated by design)
- WebSocket/SSE health streaming

## User Stories

### US-002.1: Readiness Probe with LLM Verification

**As a** Kubernetes operator,
**I want** a `/health/ready` endpoint that verifies LLM provider connectivity,
**So that** my readiness probe removes unhealthy pods from the load balancer.

**Priority:** P1
**Rationale:** LLM provider failures are the most common degraded state; credentials expire, rate limits hit, endpoints change.

#### Acceptance Criteria

**AC-002.1.1:** Readiness endpoint exists
- **Given** a running Aura web server
- **When** I send `GET /health/ready`
- **Then** I receive HTTP 200 with `"status": "healthy"` if all checks pass, or HTTP 503 with `"status": "unhealthy"` and per-subsystem details if any fail

**AC-002.1.2:** LLM provider check
- **Given** the configured LLM provider is unreachable or credentials are invalid
- **When** I send `GET /health/ready`
- **Then** I receive HTTP 503 with the `checks.llm` section showing `"status": "error"` and a sanitized diagnostic message (e.g., "auth_failed", "connection_refused", "timeout" -- never raw internal details)

**AC-002.1.3:** Multiple LLM providers checked
- **Given** multiple agent configurations with different LLM providers
- **When** I send `GET /health/ready`
- **Then** each unique provider is checked and reported individually in the response

#### Edge Cases
- Deduplicate providers with identical config (same provider type + base URL or region) to avoid redundant probes
- Bedrock has no lightweight ping API; credential validation is sufficient
- If no LLM configs exist (unlikely but possible), the `llm` section should be an empty array, not an error

### US-002.2: Liveness Probe

**As an** operator,
**I want** a `/health/live` endpoint that confirms the process is functioning,
**So that** my liveness probe restarts stuck pods.

**Priority:** P1
**Rationale:** Liveness must be cheap and always respond if the process is alive.

#### Acceptance Criteria

**AC-002.2.1:** Liveness endpoint
- **Given** a running Aura web server
- **When** I send `GET /health/live`
- **Then** I receive HTTP 200 with `{"status": "alive"}`

**AC-002.2.2:** Liveness has no dependencies
- **Given** an Aura web server with all subsystems down (LLM, MCP, vector stores unreachable)
- **When** I send `GET /health/live`
- **Then** I still receive HTTP 200 (liveness only checks process responsiveness)

### US-002.3: MCP Server Connectivity Check

**As an** operator,
**I want** the readiness check to verify all configured MCP server connections,
**So that** I know tool execution will succeed before routing traffic.

**Priority:** P1
**Rationale:** MCP server disconnects are silent failures that cause tool calls to fail at request time.

#### Acceptance Criteria

**AC-002.3.1:** MCP health check
- **Given** an HTTP Streamable MCP server is configured but unreachable
- **When** I send `GET /health/ready`
- **Then** I receive HTTP 503 with the `checks.mcp` section showing per-server status with `"status": "error"` for the unreachable server

**AC-002.3.2:** STDIO MCP servers report as ok
- **Given** a STDIO MCP server is configured
- **When** I send `GET /health/ready`
- **Then** the STDIO server reports `"status": "ok"` (STDIO processes are spawned per-request and cannot be probed without starting them)

**AC-002.3.3:** No MCP servers configured
- **Given** no MCP servers are configured in any agent
- **When** I send `GET /health/ready`
- **Then** the `checks.mcp` section is an empty object (not an error)

### US-002.4: Per-Subsystem Status

**As an** operator,
**I want** each subsystem check to report individually,
**So that** I can diagnose which dependency is failing.

**Priority:** P1
**Rationale:** Aggregate pass/fail is insufficient for diagnosis.

#### Acceptance Criteria

**AC-002.4.1:** Subsystem breakdown
- **Given** a running Aura web server with LLM, MCP, and vector store configured
- **When** I send `GET /health/ready`
- **Then** I receive a JSON response with individual status for each subsystem:
  ```json
  {
    "status": "healthy",
    "checks": {
      "llm": [
        {"provider": "bedrock", "model": "anthropic.claude-sonnet-4-20250514", "agent": "assistant", "status": "ok", "latency_ms": 45}
      ],
      "mcp": {
        "example_http": {"transport": "http_streamable", "status": "ok", "latency_ms": 12},
        "local_tools": {"transport": "stdio", "status": "ok"}
      },
      "vector_stores": {
        "docs": {"type": "qdrant", "status": "ok", "latency_ms": 8}
      }
    },
    "check_duration_ms": 52,
    "cached": false
  }
  ```
  Note: `latency_ms` is omitted for subsystems with no external call (STDIO MCP, InMemory vector stores).

**AC-002.4.2:** Vector store check
- **Given** a Qdrant vector store is configured but the Qdrant server is unreachable
- **When** I send `GET /health/ready`
- **Then** the `checks.vector_stores` section shows `"status": "error"` for that store

### US-002.5: Cached Health Checks

**As an** operator,
**I want** health checks cached for a configurable interval (default 10s),
**So that** they don't add load to upstream dependencies on every probe.

**Priority:** P2
**Rationale:** K8s probes fire frequently (every 5-15s). Without caching, each probe triggers full connectivity checks.

#### Acceptance Criteria

**AC-002.5.1:** Cache TTL
- **Given** a health check was performed less than the configured TTL ago
- **When** I send `GET /health/ready`
- **Then** I receive the cached result with `"cached": true` without re-checking dependencies

**AC-002.5.2:** Cache TTL configurable
- **Given** the `HEALTH_CHECK_CACHE_TTL_SECS` env var is set to `5`
- **When** I send `GET /health/ready` twice within 5 seconds
- **Then** the second response returns the cached result

**AC-002.5.3:** Cache expires
- **Given** the cache TTL has elapsed
- **When** I send `GET /health/ready`
- **Then** fresh health checks are executed and the cache is updated

### US-002.6: Health Check Observability

**As an** operator,
**I want** Prometheus metrics emitted for health check outcomes,
**So that** I can build alerts on subsystem health without polling the endpoint.

**Priority:** P2
**Rationale:** Prometheus-based alerting is the standard monitoring pattern.

#### Acceptance Criteria

**AC-002.6.1:** Health check Prometheus metrics
- **Given** a health check has been executed (via `/health/ready`)
- **When** I scrape `/metrics`
- **Then** I see:
  - `aura_health_ready` gauge (1.0 = healthy, 0.0 = unhealthy)
  - `aura_health_check_duration_seconds` histogram (total check duration)
  - `aura_health_subsystem_status` gauge (per-subsystem, labels: `subsystem`, `name`) set to 1.0 (ok) or 0.0 (error)

### US-002.7: Health Endpoints During Shutdown

**As an** operator,
**I want** health endpoints to remain accessible during graceful shutdown,
**So that** Kubernetes can properly drain traffic from the pod.

**Priority:** P1
**Rationale:** If `/health/ready` returns 503 during shutdown, K8s removes the pod from the LB -- this is the desired behavior. But `/health/live` must still return 200 to prevent a restart during drain.

#### Acceptance Criteria

**AC-002.7.1:** Health endpoints exempt from shutdown guard
- **Given** the Aura web server is in graceful shutdown (shutdown_token cancelled)
- **When** I send `GET /health/live` or `GET /health/ready`
- **Then** the endpoints respond normally (not rejected with 503 by the shutdown guard)

## API / Config Contract

### Endpoints

```
GET /health/live   -> 200 {"status": "alive"}
GET /health/ready  -> 200 (all ok) or 503 (any failure) with subsystem details
GET /health        -> 200 {"status": "healthy"} (unchanged, backward compatible)
```

### Readiness Response Schema

```json
{
  "status": "healthy | unhealthy",
  "checks": {
    "llm": [
      {
        "provider": "string",
        "model": "string",
        "agent": "string",
        "status": "ok | error",
        "message": "string (only on error, sanitized)",
        "latency_ms": 0
      }
    ],
    "mcp": {
      "<server_name>": {
        "transport": "http_streamable | stdio",
        "status": "ok | error",
        "message": "string (only on error, sanitized)",
        "latency_ms": 0
      }
    },
    "vector_stores": {
      "<store_name>": {
        "type": "qdrant | bedrock_kb | in_memory",
        "status": "ok | error",
        "message": "string (only on error, sanitized)",
        "latency_ms": 0
      }
    }
  },
  "check_duration_ms": 0,
  "cached": false
}
```

Fields `latency_ms` and `message` are omitted when not applicable (STDIO, InMemory, ok status).

### Configuration (CLI args / env vars)

```bash
# Health check cache TTL in seconds (default: 10)
# Recommend setting slightly less than K8s periodSeconds
HEALTH_CHECK_CACHE_TTL_SECS=10

# Individual probe timeout in seconds (default: 5)
# Must be shorter than Kubernetes probe timeoutSeconds (recommend K8s timeout >= 10s)
HEALTH_CHECK_TIMEOUT_SECS=5
```

These follow the existing pattern where server-level settings are CLI args with env var fallbacks (`STREAMING_TIMEOUT_SECS`, `SHUTDOWN_TIMEOUT_SECS`, etc.).

## Dependencies

- **Depends on:** AURA-RM-008 (Structured Error Taxonomy) -- uses ErrorCategory for classifying probe failures in logs
- **Depended on by:** None currently (AURA-RM-003 Circuit Breaker may consume health status in the future)

## Success Criteria

- Kubernetes readiness probe at `/health/ready` returns 503 when LLM credentials are invalid
- Kubernetes readiness probe at `/health/ready` returns 503 when an MCP server is down
- Kubernetes liveness probe at `/health/live` returns 200 even when all subsystems are down
- Per-subsystem diagnostic JSON enables operators to identify the failing dependency in one HTTP call
- Prometheus `aura_health_ready` gauge is scrapable for alerting
- Existing `/health` endpoint and Docker Compose healthchecks remain unchanged
- Error messages in health responses do not leak internal IPs, hostnames, or DNS names

## Open Questions

- None
