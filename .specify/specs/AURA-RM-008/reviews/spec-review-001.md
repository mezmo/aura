# Spec Review Round 1: AURA-RM-008

**Date:** 2026-04-28
**Reviewers:** Consistency Agent, Security Agent, Operability Agent

## Findings Summary

| # | Category | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Must Fix | Article I risk: changing error_type values breaks clients | FIXED — error_type values FROZEN, new `code` field is additive |
| 2 | Must Fix | StreamTermination From impl in wrong crate (circular dep) | FIXED — From impl moved to aura-web-server |
| 3 | Must Fix | Error flow diagram contradicts From impl code | FIXED — diagram corrected |
| 4 | Must Fix | thiserror rejection uses wrong rationale | FIXED — corrected to "YAGNI" |
| 5 | Must Fix | Error messages leak internal MCP server names to clients | FIXED — added sanitization layer with client_message() |
| 6 | Must Fix | No error message sanitization layer | FIXED — AuraError has internal_message + client_message() |
| 7 | Must Fix | Error flow diagram shows McpConnectionFailed but impl maps to McpToolError | FIXED — diagram matches impl |
| 8 | Should Fix | Missing service_unavailable in taxonomy | FIXED — added |
| 9 | Should Fix | Two ErrorDetail structs, spec only addresses one | FIXED — both gain code field |
| 10 | Should Fix | ToolCallError maps too broadly | DOCUMENTED — known limitation, future refinement |
| 11 | Should Fix | Article V: integration feature flag not defined | FIXED — not needed for RM-008 (unit tests only) |
| 12 | Should Fix | Dead categories (mcp_connection_failed, mcp_timeout) | DOCUMENTED — exist for future transport layer |
| 13 | Should Fix | Error code prefix enables fingerprinting | FIXED — removed AURA-E- prefix, code field uses taxonomy label directly |
| 14 | Should Fix | Test file location non-standard | FIXED — inline tests in source files |
| 15 | Should Fix | Pre-checked quality gate | FIXED — all unchecked |
| 16 | Nit | Error code 099 sentinel | FIXED — no separate error codes, just taxonomy labels |
| 17 | Nit | Enum iteration needs manual list | FIXED — ALL_CATEGORIES const |
| 18 | Nit | Constitution check omits Article IV | FIXED — added |

## Status: All findings resolved. Ready for re-review.
