# Aura Project Constitution

## Preamble

This document establishes the immutable principles that govern all development on the Aura project. Every specification, implementation, and review must comply with these articles. Amendments require explicit team consensus and a versioned update to this file.

---

## Article I: OpenAI API Compatibility

The `/v1/chat/completions` and `/v1/models` API contracts are non-negotiable. No change may break compatibility with existing OpenAI SDKs (Python, Node.js, Go, Rust) or chat frontends (LibreChat, OpenWebUI, Cursor). New functionality is additive only — new fields, new endpoints, new event types. Never remove or rename existing fields.

**Rationale:** Aura's adoption depends on drop-in compatibility with the OpenAI ecosystem. Breaking this contract breaks every client integration simultaneously.

## Article II: Crate Boundary Separation

The three-crate architecture is load-bearing:
- `aura` — runtime agent building, MCP integration, tool orchestration, vector workflows. No knowledge of HTTP, config file format, or web server concerns.
- `aura-config` — typed TOML parsing and validation. No knowledge of runtime behavior or web serving.
- `aura-web-server` — OpenAI-compatible REST/SSE serving layer. No business logic beyond request routing and response formatting.

**Rationale:** This separation enables the embeddable core (`aura` crate) to be used in any Rust application without config or web dependencies. Violating boundaries creates hidden coupling.

## Article III: No AI Co-Authorship

Never add `Co-Authored-By` lines for Claude or any AI assistant. Sign off commits as the human user. Claude cannot accept the CLA.

**Rationale:** CLA compliance. The mezmo/aura repository requires all contributors to have signed the Contributor License Agreement.

## Article IV: Conventional Commits

All commits follow the format in CLAUDE.md:
- First line: entirely lowercase, no trailing period, under 72 characters
- Format: `<type>(<optional scope>): <description>`
- Types: `feat`, `fix`, `doc`, `style`, `refactor`, `perf`, `test`, `chore`, `ci`
- Body: explains what and why
- Footer: `Ref: LOG-XXXXX` and `Signed-off-by: name <email>`

**Rationale:** Jenkins commitlint enforces this format. Non-compliant commits fail CI.

## Article V: Feature Flags for Integration Tests

New integration test suites must use the existing feature flag pattern in `aura-web-server/Cargo.toml`. Parent flag: `integration`. Suite-specific flags: `integration-<suite-name>`.

**Rationale:** Integration tests require running infrastructure (MCP servers, LLM APIs). Feature flags prevent them from running in unit test contexts and allow selective CI execution.

## Article VI: Environment Variables for Secrets

Configuration references secrets exclusively via `{{ env.VAR_NAME }}` template syntax. Never hardcode API keys, tokens, or credentials in TOML files, source code, or test fixtures.

**Rationale:** Secrets in source control are a security incident. The env var template system in `aura-config/src/env.rs` provides safe interpolation with validation.

## Article VII: Backward-Compatible Configuration

New configuration fields must have sensible defaults. Existing TOML files must continue to work without modification when Aura is upgraded. Use `#[serde(default)]` for all new fields.

**Rationale:** Deployed agents have existing config files. Forcing config changes on upgrade creates operational friction and deployment failures.

---

## Governance

**Amendment procedure:** Propose changes via PR to this file. Requires review and approval from at least one maintainer. Each amendment is versioned with a date.

**Version:** 1.0.0
**Established:** 2026-04-28
