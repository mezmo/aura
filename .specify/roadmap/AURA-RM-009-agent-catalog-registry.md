# AURA-RM-009: Agent Catalog / Registry

**Priority:** P3 (Lower)
**Status:** Not Started
**Dependencies:** None (standalone)
**Depended on by:** None
**Affected Crates:** New subsystem
**Complexity:** High

## Rationale

In multi-team environments, teams create agents independently. There is no way to discover, share, or version agent configurations across teams. As the number of agents grows, a central registry prevents duplication and enables cross-team collaboration.

## User Stories

### US-009.1: Publish Agent Configs

**As a** platform engineer,
**I want** a central registry where teams can publish agent TOML configs with metadata,
**So that** other teams can discover and reuse agents.

### US-009.2: Search by Capability

**As a** developer,
**I want** to search the registry by capability (e.g., "kubernetes," "log analysis"),
**So that** I can find relevant agents without browsing all configs.

### US-009.3: Versioned Configs

**As a** platform engineer,
**I want** agent configs to be versioned,
**So that** I can roll back to a previous version if a new config causes issues.
