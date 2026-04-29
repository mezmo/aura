# Implementation Plan: AURA-RM-001 Prometheus Metrics Endpoint

## Summary

Add `/metrics` endpoint to `aura-web-server` using `metrics` + `metrics-exporter-prometheus` crates. Record request duration, token usage, tool duration, errors, in-flight count, and MCP connection state. All in `aura-web-server` only (Article II).

## Implementation Order

1. Add dependencies to `aura-web-server/Cargo.toml`
2. Create `metrics.rs` with init + all recording functions + handler
3. Register `/metrics` route in main.rs (outside shutdown_guard)
4. Instrument request entry/exit in handlers.rs (duration + in-flight gauge)
5. Instrument token recording from UsageState at request completion
6. Instrument tool duration from handler-level tool_complete events
7. Instrument error recording using ErrorCategory (from RM-008)
8. Add MCP server connection state gauge
9. Add `integration-metrics` feature flag
10. Unit + integration tests

## Dependencies

- AURA-RM-008 must be complete (ErrorCategory for error labels)

## Estimated Scope

- 1 new file (metrics.rs)
- 4 modified files (Cargo.toml, main.rs, handlers.rs, types.rs)
- ~250 lines of new code + ~200 lines of tests
