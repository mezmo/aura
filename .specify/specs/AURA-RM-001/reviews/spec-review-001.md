# Spec Review Round 1: AURA-RM-001

**Date:** 2026-04-28
**Reviewers:** Consistency Agent, Security Agent, Operability Agent

## Findings Summary

| # | Category | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Must Fix | Article II: metrics recording in aura crate violates boundary | FIXED — all metrics in aura-web-server only, tool timing via event broker |
| 2 | Must Fix | /metrics exposed without access control | FIXED — document as sensitive, default localhost, AURA_METRICS_BIND_ADDRESS planned |
| 3 | Must Fix | In-flight gauge set-after-read race | FIXED — use gauge.increment(1.0)/decrement(1.0) directly |
| 4 | Must Fix | ActiveRequestTracker ordering change | FIXED — atomic orderings unchanged, gauge is separate |
| 5 | Must Fix | Article V: integration-metrics feature flag not defined | FIXED — added to Cargo.toml spec |
| 6 | Must Fix | tool label unbounded cardinality | FIXED — 100-tool cap + 64-char length guard → _other |
| 7 | Should Fix | Histogram missing sub-100ms bucket | FIXED — added 0.025 bucket |
| 8 | Should Fix | MCP tool histogram upper bound too low | FIXED — extended to 60s |
| 9 | Should Fix | No AURA_METRICS_ENABLED kill switch | FIXED — added env var, default true |
| 10 | Should Fix | Missing MCP server connection state gauge | FIXED — added US-001.6 / AC-001.6.1 |
| 11 | Should Fix | No process metrics | DOCUMENTED — out of scope, use node exporter |
| 12 | Should Fix | Scrape latency not in quality spec | FIXED — added TC-001.P.1 performance test |
| 13 | Should Fix | UsageState extraction unclear | FIXED — added usage_state.snapshot() in handler code |
| 14 | Should Fix | /metrics route placement vs shutdown_guard | FIXED — registered outside shutdown_guard |
| 15 | Should Fix | init_metrics() panics on error | FIXED — uses .expect() with clear messages |
| 16 | Should Fix | Agent label in metrics may contain spaces | DOCUMENTED — operator choice via config |
| 17 | Should Fix | AC-001.1.3 about auth exemption untestable | FIXED — changed to kill switch test instead |
| 18 | Should Fix | Provider labels hardcoded | FIXED — sourced from Agent::get_provider_info() |
| 19 | Should Fix | Test count hardcoded | FIXED — uses "existing tests" not number |
| 20 | Should Fix | Mock infra not referenced | FIXED — references aura-test-utils |
| 21 | Nit | promtool requirement | DOCUMENTED — "if available in CI" |
| 22 | Nit | Render caching | DOCUMENTED — future optimization |
| 23 | Nit | .unwrap() in init | FIXED — changed to .expect() |

## Status: All findings resolved. Ready for re-review.
