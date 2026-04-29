# Spec Review Round 1: AURA-RM-002

**Date:** 2026-04-28
**Reviewers:** Consistency Agent, Architecture Agent, Security Agent, Operability Agent, Testing Agent

## Findings Summary

| # | Perspective | Category | Finding | Resolution |
|---|-----------|----------|---------|------------|
| 1 | Consistency | Must Fix | Article III missing from constitution compliance check | FIXED -- added to architecture spec |
| 2 | Consistency | Must Fix | `NotConfigured` variant in description but absent from Rust type definition | FIXED -- removed from description; empty collections used instead (matches product spec) |
| 3 | Consistency | Must Fix | `degraded` status in product spec response schema but no rule produces it | FIXED -- removed from product spec schema; `Degraded` remains in Rust enum as reserved (documented in arch spec) |
| 4 | Consistency | Must Fix | Roadmap JSON schema conflicts with product spec (array vs object for llm) | DOCUMENTED -- product spec is authoritative; roadmap is Phase 1 output |
| 5 | Consistency | Must Fix | Prometheus metrics referenced in scope/success criteria but no AC, definition, or tests | FIXED -- added US-002.6 with AC-002.6.1, full metric definitions in arch spec, TC-002.6.1.1 in quality spec |
| 6 | Security | Must Fix | Health response error messages may leak internal IPs/hostnames/DNS names | FIXED -- added error message sanitization with fixed safe categories (connection_refused, auth_failed, timeout, dns_error, probe_error). Raw details logged at WARN only. |
| 7 | Operability | Must Fix | No AC or test for shutdown guard exemption of `/health/*` paths | FIXED -- added US-002.7 with AC-002.7.1 and TC-002.7.1.1 |
| 8 | Testing | Must Fix | No test for HTTP 503 on unhealthy readiness | FIXED -- added TC-002.1.1.2 testing 503 status code mapping |
| 9 | Testing | Must Fix | "No LLM configs" edge case explicitly called out but untested | FIXED -- added TC-002.1.E.1 |
| 10 | Testing | Must Fix | Deduplication test insufficient (no call count assertion) | FIXED -- TC-002.1.3.1 now asserts on unique target count |
| 11 | Consistency | Should Fix | `aura_config::Config` used in aura crate but aura doesn't depend on aura-config | FIXED -- clarified that `run_health_check()` accepts `aura_config::Config` from web-server layer; probe functions use aura crate types |
| 12 | Consistency | Should Fix | Shutdown guard update omits `/metrics` exemption | FIXED -- shows full exemption: `/health` OR starts_with `/health/` OR `/metrics` |
| 13 | Architecture | Should Fix | Write-lock starvation when cache expires under concurrent requests | DOCUMENTED -- write-lock duration bounded by probe_timeout (5s). Acceptable for K8s probe concurrency (1-3). Added to Risks and Alternatives. |
| 14 | Architecture | Should Fix | MCP probe sends GET which may not be valid for MCP SSE servers | FIXED -- documented that any HTTP response (including 404/405) = reachable; only connection error = down |
| 15 | Architecture | Should Fix | Anthropic probe endpoint incorrect (`/v1/models` doesn't exist) | FIXED -- changed to `GET /v1/messages` which returns 405, confirming reachability |
| 16 | Architecture | Should Fix | 401 treated as "reachable" contradicts product spec (invalid creds = 503) | FIXED -- 401/403 now maps to `SubsystemStatus::Error { message: "auth_failed" }`, making status Unhealthy/503 |
| 17 | Architecture | Should Fix | `latency_ms` optionality mismatch between product and arch specs | FIXED -- product spec notes latency_ms is omitted for STDIO/InMemory; arch spec uses `Option` with `skip_serializing_if` |
| 18 | Architecture | Should Fix | No timeout wrapping on individual probe calls | FIXED -- added explicit timeout wrapping section in arch spec; each probe wrapped in `tokio::time::timeout` |
| 19 | Security | Should Fix | API keys sent in MCP probe requests | FIXED -- MCP probes now send GET WITHOUT auth headers (connectivity check only) |
| 20 | Security | Should Fix | No rate limiting on health endpoints | DOCUMENTED -- cache serializes concurrent misses; rate limiting deferred to future hardening |
| 21 | Security | Should Fix | MCP static headers (potential auth tokens) sent in probe | FIXED -- MCP probes do not send auth headers (see #19) |
| 22 | Operability | Should Fix | Default cache TTL may mask flapping with K8s probes | DOCUMENTED -- recommended setting TTL < K8s periodSeconds in product spec config section |
| 23 | Operability | Should Fix | No logging requirements specified | FIXED -- added Logging section to arch spec (INFO transitions, WARN failures, DEBUG probes) |
| 24 | Operability | Should Fix | Cache stampede risk | DOCUMENTED -- double-checked locking bounds contention; added to Risks section |
| 25 | Operability | Should Fix | `degraded` status defined but never produced | FIXED -- removed from observable schema; reserved in Rust enum only (see #3) |
| 26 | Operability | Should Fix | Probe timeout vs K8s timeout interaction | FIXED -- added startup warning when timeout >= 10s; documented recommended K8s config |
| 27 | Testing | Should Fix | No `HealthStatus::Degraded` serialization test | FIXED -- added TC-002.SER.1 for all HealthStatus variant serialization |
| 28 | Testing | Should Fix | No "no vector stores" empty case test | FIXED -- added TC-002.4.E.1 |
| 29 | Testing | Should Fix | No InMemory vector store test | FIXED -- added TC-002.4.2.2 |
| 30 | Testing | Should Fix | No probe timeout behavior test | DOCUMENTED -- probes use `tokio::time::timeout` wrapping; tests use short timeouts against non-routable addresses |
| 31 | Testing | Should Fix | TC-002.2.2.1 not a behavioral test | FIXED -- rewritten as behavioral test verifying liveness returns 200 regardless of HealthCheckService state |
| 32 | Testing | Should Fix | Cache tests need mocking strategy | FIXED -- added `set_cache_for_test()` method; documented in Test Infrastructure section |
| 33 | Testing | Should Fix | Time-dependent cache tests may be flaky | DOCUMENTED -- uses 100ms TTL with generous tolerance; `std::time::Instant` is sufficient |
| 34 | Testing | Should Fix | No backward compat regression test for /health | FIXED -- added TC-002.BC.1 |
| 35 | Testing | Should Fix | No shutdown guard exemption test | FIXED -- added TC-002.7.1.1 (see #7) |
| 36 | Architecture | Nit | HashMap produces non-deterministic JSON key order | FIXED -- changed to `BTreeMap` for `mcp` and `vector_stores` |
| 37 | Architecture | Nit | Gemini in probe strategy table but not a supported provider | FIXED -- removed Gemini row |
| 38 | Consistency | Nit | Roadmap says "env var or TOML" but specs say "CLI args / env vars" | DOCUMENTED -- product spec is authoritative |
| 39 | Consistency | Nit | Network-dependent unit tests not classified | DOCUMENTED -- tests noted as "Unit (with network)" using non-routable addresses |
| 40 | Security | Nit | `agent` field in LLM results leaks internal naming | ACCEPTED -- agent names are already exposed via `/v1/models` endpoint (model IDs = agent names/aliases) |
| 41 | Operability | Nit | `check_duration_ms` semantics unclear for cached responses | DOCUMENTED -- cached responses retain original `check_duration_ms` |
| 42 | Operability | Nit | No version/instance in health response | DEFERRED -- may add in future; not critical for initial release |
| 43 | Testing | Nit | Traceability doesn't cover success criteria separately | FIXED -- success criteria map to existing test cases; backward compat has explicit TC-002.BC.1 |

## Status: All Must Fix and Should Fix findings resolved. Nits addressed or accepted with rationale. Ready for implementation.
