# Implementation Plan: AURA-RM-002 Deep Health and Readiness Checks

## Summary

Add `/health/live` (liveness) and `/health/ready` (readiness with per-subsystem checks) endpoints to the Aura web server. Core probe logic in `aura` crate, HTTP handlers + cache in `aura-web-server`. Lightweight probes for LLM providers (HTTP model-listing), MCP servers (HTTP connectivity), and vector stores (REST health endpoints). Results cached with configurable TTL. Prometheus metrics for health status.

## Implementation Order

1. New module `health.rs` in `aura` crate: types + probe functions + aggregation
2. Export from `lib.rs`
3. New module `health.rs` in `aura-web-server`: HealthCheckService + cache + HTTP handlers
4. AppState + routes + CLI args in `main.rs`
5. Shutdown guard update
6. Prometheus metrics in `metrics.rs`
7. Unit tests (inline in both crates)

## Estimated Scope

- 2 new files (`crates/aura/src/health.rs`, `crates/aura-web-server/src/health.rs`)
- 4 modified files (`lib.rs`, `main.rs`, `types.rs`, `metrics.rs`)
- ~400 lines of new code + ~300 lines of tests
