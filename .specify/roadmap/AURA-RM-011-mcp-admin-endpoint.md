# AURA-RM-011: MCP Server Admin Endpoint

**Priority:** P3 (Lower)
**Status:** Not Started
**Dependencies:** AURA-RM-001 (Metrics), AURA-RM-003 (Circuit Breaker)
**Depended on by:** None
**Affected Crates:** `aura-web-server`, `aura`
**Complexity:** Medium

## Rationale

Operators cannot see which MCP tools are connected, their latency characteristics, or error rates without digging through logs. This provides a human-readable admin view complementing the machine-readable metrics endpoint (RM-001).

## User Stories

### US-011.1: Connected MCP Server Listing

**As an** operator,
**I want** a `/admin/mcp` endpoint that lists all connected MCP servers, their tools, and current status,
**So that** I can verify the agent has the tools it needs.

#### Acceptance Criteria

**AC-011.1.1:** Admin endpoint
- **Given** a running Aura web server with MCP servers configured
- **When** I send `GET /admin/mcp`
- **Then** I receive a JSON response listing each server, its transport type, tool count, and connection status

### US-011.2: Per-Tool Latency and Errors

**As an** operator,
**I want** to see per-tool latency and error stats,
**So that** I can identify problematic tools.

### US-011.3: Circuit Breaker State Visibility

**As an** operator,
**I want** to see circuit breaker state on the admin endpoint (AURA-RM-003),
**So that** I can understand why certain tools are unavailable.
