# AURA-RM-002: Deep Health and Readiness Checks

**Priority:** P0 (Critical)
**Status:** Implemented
**Dependencies:** AURA-RM-008 (Structured Error Taxonomy)
**Depended on by:** None
**Affected Crates:** `aura-web-server`, `aura`
**Complexity:** Medium-High

## Rationale

The current `/health` endpoint returns a static `{"status": "healthy"}` regardless of actual system state. Kubernetes liveness/readiness probes never detect a degraded instance — failed LLM credentials, disconnected MCP servers, or unreachable vector stores all present as "healthy." This causes traffic to route to broken pods.

## User Stories

### US-002.1: Readiness Probe with LLM Verification

**As a** Kubernetes operator,
**I want** a `/health/ready` endpoint that verifies LLM provider connectivity,
**So that** my readiness probe removes unhealthy pods from the load balancer.

#### Acceptance Criteria

**AC-002.1.1:** Readiness endpoint exists
- **Given** a running Aura web server
- **When** I send `GET /health/ready`
- **Then** I receive 200 if all checks pass, or 503 with details if any fail

**AC-002.1.2:** LLM provider check
- **Given** the configured LLM provider credentials are invalid
- **When** I send `GET /health/ready`
- **Then** I receive 503 with `"llm": {"status": "error", "message": "..."}`

### US-002.2: Liveness Probe

**As an** operator,
**I want** a `/health/live` endpoint that confirms the process is functioning,
**So that** my liveness probe restarts stuck pods.

#### Acceptance Criteria

**AC-002.2.1:** Liveness endpoint
- **Given** a running Aura web server
- **When** I send `GET /health/live`
- **Then** I receive 200 with `{"status": "alive"}`

### US-002.3: MCP Server Connectivity Check

**As an** operator,
**I want** the readiness check to verify all configured MCP server connections,
**So that** I know tool execution will succeed before routing traffic.

#### Acceptance Criteria

**AC-002.3.1:** MCP health check
- **Given** an MCP server is configured but unreachable
- **When** I send `GET /health/ready`
- **Then** I receive 503 with `"mcp": {"server_name": {"status": "error", "message": "..."}}`

### US-002.4: Per-Subsystem Status

**As an** operator,
**I want** each subsystem check to report individually,
**So that** I can diagnose which dependency is failing.

#### Acceptance Criteria

**AC-002.4.1:** Subsystem breakdown
- **Given** a running Aura web server with LLM, MCP, and vector store configured
- **When** I send `GET /health/ready`
- **Then** I receive a JSON response with individual status for each subsystem:
  ```json
  {
    "status": "healthy",
    "checks": {
      "llm": {"status": "ok", "provider": "bedrock"},
      "mcp": {
        "server_1": {"status": "ok", "tools": 5},
        "server_2": {"status": "error", "message": "connection refused"}
      },
      "vector_stores": {
        "docs": {"status": "ok", "type": "qdrant"}
      }
    }
  }
  ```

### US-002.5: Cached Health Checks

**As an** operator,
**I want** health checks cached for a configurable interval (default 10s),
**So that** they don't add load to upstream dependencies on every probe.

#### Acceptance Criteria

**AC-002.5.1:** Cache TTL
- **Given** a health check was performed less than 10 seconds ago
- **When** I send `GET /health/ready`
- **Then** I receive the cached result without re-checking dependencies

## Existing Infrastructure

- `/health` endpoint at `handlers.rs` line 634 (static response)
- `McpManager` holds `Arc<RunningService>` clients — implement ping/list-tools probe
- Helm chart has liveness/readiness probes configured pointing to `/health`

## Edge Cases

- Health checks must not require authentication
- Cache TTL must be configurable via env var or TOML
- If no MCP servers are configured, the MCP check should report "not configured" (not "error")
- Health check timeout must be shorter than Kubernetes probe timeout
