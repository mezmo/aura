//! The crate's integration tests, linked as one binary. Cargo does not
//! discover files in this directory, so a new suite needs a `mod` line here.
//! Each suite gates itself on its `integration-*` feature.

mod common;

mod a2a_test;
mod aura_events_test;
mod cancellation_notification_test;
mod cancellation_test;
mod concurrent_streaming_isolation_test;
mod deprecation_shim;
mod fastmcp_structured_content_test;
mod file_session_store_test;
mod header_forwarding_tests;
mod hitl_header_forwarding_tests;
mod mcp_progress_test;
mod orchestration_events_test;
mod proactive_tool_ui_tests;
mod progress_disconnect_test;
mod redis_session_store_test;
mod scratchpad_test;
mod sre_orchestration_test;
mod sse_parity;
mod streaming_tests;
