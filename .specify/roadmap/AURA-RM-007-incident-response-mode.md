# AURA-RM-007: Incident Response Mode

**Priority:** P2 (Medium)
**Status:** Not Started
**Dependencies:** AURA-RM-008 (Structured Error Taxonomy)
**Depended on by:** None
**Affected Crates:** `aura-web-server`, `aura`, potentially new `aura-integrations`
**Complexity:** High

## Rationale

Aura is deployed for SRE use cases with PagerDuty and Datadog MCP tools. Tying an agent session to a live incident creates an auditable timeline of agent actions during the incident. This is the killer SRE use case — an on-call assistant that has context, takes actions, and produces a post-incident report automatically.

## User Stories

### US-007.1: Incident Context Injection

**As an** SRE,
**I want** to pass a PagerDuty incident ID when starting a chat session,
**So that** all agent actions are correlated with the incident timeline.

#### Acceptance Criteria

**AC-007.1.1:** Incident ID in metadata
- **Given** a chat request with `metadata.incident_id = "P12345"`
- **When** the agent processes the request
- **Then** all tool calls, events, and logs include the incident_id for correlation

### US-007.2: Automatic Incident Context Retrieval

**As an** SRE,
**I want** the agent to automatically retrieve incident context when an incident ID is provided,
**So that** it has immediate situational awareness.

#### Acceptance Criteria

**AC-007.2.1:** Auto-context
- **Given** a PagerDuty MCP server is configured and an incident_id is provided
- **When** the agent starts processing
- **Then** it retrieves the incident details (alerts, affected services, timeline) before responding

### US-007.3: Post-Incident Report

**As an** incident commander,
**I want** a summary of all tools the agent invoked, their results, and recommendations,
**So that** I can include agent actions in the post-mortem.

#### Acceptance Criteria

**AC-007.3.1:** Session summary
- **Given** an incident-mode session has completed
- **When** the session ends
- **Then** a structured summary is available via the API or as a final event

## Edge Cases

- Incident ID must be optional — sessions without incident_id work normally
- If PagerDuty MCP is not configured, incident_id is stored for correlation but auto-context is skipped
