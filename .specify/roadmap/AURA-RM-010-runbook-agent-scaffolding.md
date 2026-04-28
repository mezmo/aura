# AURA-RM-010: Runbook-Driven Agent Scaffolding

**Priority:** P3 (Lower)
**Status:** Not Started
**Dependencies:** None (standalone)
**Depended on by:** None
**Affected Crates:** New tooling (CLI or script)
**Complexity:** Medium

## Rationale

SRE teams have existing runbooks in Markdown, Confluence, or wikis. Manually translating runbooks into TOML agent configs requires learning the config format and writing system prompts from scratch. Automatically generating TOML from runbooks lowers the barrier to agent creation.

## User Stories

### US-010.1: Generate Config from Runbook

**As an** SRE,
**I want** to provide a runbook and have Aura generate a TOML agent config,
**So that** I can create agents without learning TOML syntax.

### US-010.2: Runbook as Agent Context

**As an** SRE,
**I want** the generated config to include the runbook content as context,
**So that** the agent follows documented procedures during incidents.
