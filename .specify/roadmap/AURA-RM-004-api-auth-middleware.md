# AURA-RM-004: API Authentication Middleware

**Priority:** P1 (High)
**Status:** Not Started
**Dependencies:** AURA-RM-008 (Structured Error Taxonomy)
**Depended on by:** None
**Affected Crates:** `aura-web-server`, `aura-config`
**Complexity:** Low-Medium

## Rationale

The Aura API is currently wide open — no authentication on any endpoint. While it's designed for deployment behind a reverse proxy, defense-in-depth requires the ability to validate API keys at the application layer. Every team deploying Aura currently has to solve auth separately.

## User Stories

### US-004.1: API Key Authentication

**As a** platform operator,
**I want** to configure API key authentication via TOML or env vars,
**So that** I can restrict access without requiring a reverse proxy.

#### Acceptance Criteria

**AC-004.1.1:** Bearer token validation
- **Given** auth is configured with one or more API keys
- **When** a request arrives with `Authorization: Bearer <valid-key>`
- **Then** the request proceeds to the handler

**AC-004.1.2:** Rejection of invalid tokens
- **Given** auth is configured
- **When** a request arrives with an invalid or missing Authorization header
- **Then** the request is rejected with 401 Unauthorized before reaching handler logic

### US-004.2: Unauthenticated Health Endpoints

**As a** platform operator,
**I want** the health and metrics endpoints to remain unauthenticated,
**So that** Kubernetes probes and Prometheus scrapers work without credentials.

#### Acceptance Criteria

**AC-004.2.1:** Health exempt
- **Given** auth is configured
- **When** I send `GET /health` without an Authorization header
- **Then** I receive 200 (not 401)

### US-004.3: Key Rotation Support

**As an** operator,
**I want** to support multiple API keys (for rotation),
**So that** I can rotate keys without downtime.

#### Acceptance Criteria

**AC-004.3.1:** Multiple keys
- **Given** auth is configured with keys ["key-1", "key-2"]
- **When** a request arrives with `Authorization: Bearer key-1` or `Bearer key-2`
- **Then** both are accepted

### US-004.4: Optional Auth (Disabled by Default)

**As a** developer,
**I want** auth to be optional and disabled by default,
**So that** the quick-start experience is not degraded.

#### Acceptance Criteria

**AC-004.4.1:** Default off
- **Given** no auth section in TOML config
- **When** the server starts
- **Then** all requests are accepted without authentication

## Configuration Example

```toml
[auth]
api_keys = ["{{ env.AURA_API_KEY_1 }}", "{{ env.AURA_API_KEY_2 }}"]
exclude_paths = ["/health", "/health/ready", "/health/live", "/metrics"]
```
