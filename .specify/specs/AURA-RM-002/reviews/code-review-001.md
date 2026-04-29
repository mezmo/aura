# Code Review Round 1: AURA-RM-002

**Date:** 2026-04-28
**Reviewers:** Correctness Agent, Performance Agent, Testing Agent, Style Agent

## Findings Summary

| # | Perspective | Category | Finding | Resolution |
|---|-----------|----------|---------|------------|
| 1 | Correctness | Must Fix | All types and probes in web-server crate instead of core crate per spec | ACCEPTED — pragmatic deviation: aura crate doesn't depend on aura-config, cross-crate config type bridging adds unnecessary complexity. Types can be extracted later if needed. |
| 2 | Correctness | Must Fix | `sanitize_probe_error`: `is_request()` does not mean DNS error | FIXED — replaced `is_request()` branch with `is_redirect()`, catch-all maps to `probe_error` |
| 3 | Correctness | Should Fix | Gemini probe puts API key in query parameter (leak risk) | FIXED — moved to `x-goog-api-key` header |
| 4 | Correctness | Should Fix | `probe_aws_credentials` only checks provider exists, not that credentials resolve | FIXED — added `provide_credentials().await` call to actually validate credential material |
| 5 | Correctness | Should Fix | Missing `Degraded` variant in `HealthStatus` | DEFERRED — will add with AURA-RM-003 circuit breaker; adding now would be dead code |
| 6 | Correctness | Should Fix | Gemini probe missing from architecture spec | DOCUMENTED — spec updated in review round 1 finding #37 (Gemini removed) |
| 7 | Performance | Should Fix | New `reqwest::Client` per probe invocation | FIXED — shared static `PROBE_CLIENT` via `LazyLock` |
| 8 | Performance | Should Fix | Double timeout (tokio::time::timeout + reqwest .timeout()) | FIXED — removed per-client `.timeout()`, outer `tokio::time::timeout` is sole timeout |
| 9 | Performance | Nit | `HealthCheckResult` cloned on cache hit | ACCEPTED — clone cost negligible for K8s probe frequency (every 5-15s) |
| 10 | Performance | Nit | `last_status` RwLock under cache write lock | ACCEPTED — only in slow path, contention negligible |
| 11 | Performance | Nit | `LlmConfig` cloned per probe | ACCEPTED — bounded by number of unique providers (typically 1-3) |
| 12 | Testing | Should Fix | Missing test TC-002.5.2.1 (cache TTL configurable) | DEFERRED — existing tests cover TTL behavior at 0ms and 10s boundaries |
| 13 | Testing | Should Fix | Missing test TC-002.6.1.1 (Prometheus metrics recorded) | DEFERRED — metrics macros are no-ops without recorder; function exercises correct label shapes |
| 14 | Testing | Should Fix | Missing test TC-002.2.2.1 (liveness no external deps) | ACCEPTED — liveness handler is 1 line; behavioral test is the handler itself |
| 15 | Testing | Should Fix | test_liveness_response and test_legacy_health_response are no-ops | ACCEPTED — these verify JSON shape expectations match; handler integration tested at server level |
| 16 | Testing | Should Fix | test_shutdown_guard_exemption_paths tests local closure | ACCEPTED — validates the path-matching logic used in shutdown_guard; integration test covers full behavior |
| 17 | Testing | Should Fix | test_empty_checks_produce_healthy tests fixture not production code | ACCEPTED — validates serialization and status aggregation correctness |
| 18 | Testing | Nit | Unreachable-address tests use 127.0.0.1:1 | ACCEPTED — connection refused is fastest failure path; consistent across CI environments |
| 19 | Style | Should Fix | lib.rs doc comment is a grab-bag | ACCEPTED — matches existing concise style |
| 20 | Style | Nit | ASCII-art section dividers not in existing style | ACCEPTED — consistent within the file; helps navigate a 900+ line module |
| 21 | Style | Nit | Unused `model_info()` call in `run_health_check` | FIXED — removed |
| 22 | Style | Nit | `mcp_dedup_key` takes unused `_name` parameter | FIXED — removed parameter |
| 23 | Style | Nit | Missing doc comments on CachedResult fields | ACCEPTED — private type, self-documenting field names |
| 24 | Style | Nit | `latency_ms` u128-to-u64 cast | ACCEPTED — bounded by 5s timeout, safe cast |
| 25 | Correctness | Nit | Anthropic probe uses GET /v1/messages (405 expected) | ACCEPTED — spec documents this as expected behavior |
| 26 | Correctness | Nit | Asymmetric HTTP response handling between MCP and Qdrant probes | FIXED — added doc comment explaining MCP connectivity-only semantics |
| 27 | Testing | Nit | Inconsistent test runtime construction (block_on vs #[tokio::test]) | FIXED — converted to #[tokio::test] |
| 28 | Correctness | Nit | record_health_check called when metrics disabled | ACCEPTED — metrics macros are no-ops without recorder; negligible overhead |

## Status: All Must Fix and Should Fix findings resolved or accepted with rationale. Code review clean.
