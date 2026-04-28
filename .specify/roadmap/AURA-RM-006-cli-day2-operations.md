# AURA-RM-006: CLI for Day-2 Operations

**Priority:** P2 (Medium)
**Status:** In Progress (branch: justingross/LOG-23587-add-aura-cli)
**Dependencies:** None (low coupling)
**Depended on by:** None
**Affected Crates:** New `aura-cli` crate
**Complexity:** Medium

## Rationale

SREs live in terminals. Day-2 operations (config validation, agent listing, health checks, interactive chat) currently require curl commands or a running chat UI. A CLI enables quick agent interaction and operational tasks directly from the terminal.

## User Stories

### US-006.1: Config Validation

**As a** developer,
**I want** `aura validate config.toml` to parse and validate a config without starting the server,
**So that** I can catch errors in CI before deployment.

### US-006.2: Health Check

**As an** operator,
**I want** `aura health --url http://localhost:8080` to call the health endpoint and format the result,
**So that** I can check status from the CLI.

### US-006.3: Interactive Chat

**As a** developer,
**I want** `aura chat --agent devops --url http://localhost:8080` to start an interactive session,
**So that** I can test agents quickly without a UI.

### US-006.4: Agent Discovery

**As an** operator,
**I want** `aura agents --url http://localhost:8080` to list all configured agents,
**So that** I can discover available agents.

## Notes

This item is tracked for completeness. Active development is on the `justingross/LOG-23587-add-aura-cli` branch by Justin Gross.
