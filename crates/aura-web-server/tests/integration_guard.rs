//! Featureless `cargo test -p aura-web-server` compiles every `#![cfg(feature =
//! "integration-*")]` suite away; this ignored test surfaces that instead of a misleading green "0 passed".

#[cfg(not(any(
    feature = "integration",
    feature = "integration-a2a",
    feature = "integration-streaming",
    feature = "integration-header-forwarding",
    feature = "integration-mcp",
    feature = "integration-events",
    feature = "integration-cancellation",
    feature = "integration-progress",
    feature = "integration-vector",
    feature = "integration-orchestration",
    feature = "integration-orchestration-sre",
    feature = "integration-scratchpad",
)))]
#[test]
#[ignore = "no integration feature enabled — the aura-web-server suites need one; \
            prefer the make test-integration-* targets, or pass --features integration"]
fn integration_suite_requires_a_feature_flag() {}
