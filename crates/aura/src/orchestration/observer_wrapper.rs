//! Observer tool wrapper for orchestration mode.
//!
//! Wraps MCP tools to emit events to a `ToolCallObserver` for real-time
//! visibility into worker tool execution. Events are forwarded to the
//! orchestrator's SSE stream for client consumption.
//!
//! This wrapper is only used for orchestrator workers, not regular agents.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::tool_call_observer::{RetryHint, ToolCallObserver, ToolEvent};
use crate::tool_wrapper::{
    ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
};
use rig::tool::ToolError;

/// Counter for generating unique tool call IDs within a process.
static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique tool call ID.
fn generate_tool_call_id(task_id: usize, tool_name: &str) -> String {
    let counter = CALL_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("task{}_{}_{}", task_id, tool_name, counter)
}

/// Tool wrapper that emits events to a `ToolCallObserver`.
///
/// This wrapper provides real-time visibility into tool execution by emitting
/// `ToolEvent::CallStarted` when a tool begins execution and
/// `ToolEvent::CallCompleted` when it finishes.
///
/// Events are emitted to a broadcast channel that the orchestrator can
/// subscribe to for forwarding to the SSE event stream.
#[derive(Clone)]
pub struct ObserverWrapper {
    /// The observer to emit events to
    observer: ToolCallObserver,
    /// Task ID for correlating tool calls with tasks
    task_id: usize,
}

impl ObserverWrapper {
    pub fn new(observer: ToolCallObserver, task_id: usize) -> Self {
        Self { observer, task_id }
    }
}

#[async_trait]
impl ToolWrapper for ObserverWrapper {
    fn transform_args(&self, args: Value, ctx: &ToolCallContext) -> TransformArgsResult {
        // Generate a unique tool call ID
        let tool_call_id = generate_tool_call_id(self.task_id, &ctx.tool_name);

        // Emit CallStarted event
        self.observer.emit(ToolEvent::call_started(
            &tool_call_id,
            &ctx.tool_name,
            &ctx.tool_initiator_id,
            args.clone(),
        ));

        // Store tool_call_id in extracted data for use in on_complete
        let extracted = serde_json::json!({
            "observer_tool_call_id": tool_call_id
        });

        TransformArgsResult {
            args,
            extracted: Some(extracted),
        }
    }

    fn transform_output(
        &self,
        output: String,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> TransformOutputResult {
        // Output passes through unchanged
        TransformOutputResult::new(output)
    }

    fn handle_error(
        &self,
        error: ToolError,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> ToolError {
        // Error passes through unchanged
        error
    }

    async fn on_complete(
        &self,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
        result: Result<&str, &str>,
        duration_ms: u64,
    ) {
        // Extract tool_call_id from the extracted data
        let tool_call_id = extracted
            .and_then(|v| {
                // Handle both direct object and array from ComposedWrapper
                if let Some(arr) = v.as_array() {
                    // Find the object with observer_tool_call_id
                    arr.iter()
                        .find_map(|item| item.get("observer_tool_call_id"))
                        .and_then(|v| v.as_str())
                } else {
                    v.get("observer_tool_call_id").and_then(|v| v.as_str())
                }
            })
            .unwrap_or_else(|| {
                // Fallback: generate a new ID (shouldn't happen in normal flow)
                tracing::warn!(
                    "observer_tool_call_id not found in extracted data for {}",
                    ctx.tool_name
                );
                "unknown"
            });

        // Emit CallCompleted event (full output — truncation happens at SSE handler layer)
        let event = match result {
            Ok(output) => ToolEvent::call_completed_success(tool_call_id, output, duration_ms),
            Err(err) => {
                let retry_hint = RetryHint::from_error_message(err);
                ToolEvent::call_completed_error(tool_call_id, err, Some(retry_hint), duration_ms)
            }
        };

        self.observer.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_tool_call_id() {
        let id1 = generate_tool_call_id(0, "search");
        let id2 = generate_tool_call_id(0, "search");
        let id3 = generate_tool_call_id(1, "fetch");

        // IDs should be unique
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);

        // IDs should contain task and tool info
        assert!(id1.contains("task0"));
        assert!(id1.contains("search"));
        assert!(id3.contains("task1"));
        assert!(id3.contains("fetch"));
    }

    #[tokio::test]
    async fn test_observer_wrapper_emits_events() {
        let (observer, mut rx) = ToolCallObserver::new(8);
        let wrapper = ObserverWrapper::new(observer, 42);

        let ctx = ToolCallContext::new("test_tool");
        let args = serde_json::json!({"param": "value"});

        // Call transform_args (should emit CallStarted)
        let result = wrapper.transform_args(args, &ctx);

        // Verify CallStarted was emitted
        let event = rx.recv().await.unwrap();
        match event {
            ToolEvent::CallStarted {
                tool_name,
                tool_call_id,
                tool_initiator_id,
                ..
            } => {
                assert_eq!(tool_name, "test_tool");
                assert!(tool_call_id.contains("task42"));
                // Default initiator ID is empty
                assert_eq!(tool_initiator_id, "");
            }
            _ => panic!("Expected CallStarted event"),
        }

        // Call on_complete (should emit CallCompleted)
        wrapper
            .on_complete(&ctx, result.extracted.as_ref(), Ok("success"), 100)
            .await;

        let event = rx.recv().await.unwrap();
        match event {
            ToolEvent::CallCompleted {
                duration_ms,
                result,
                ..
            } => {
                assert_eq!(duration_ms, 100);
                assert!(result.is_success());
            }
            _ => panic!("Expected CallCompleted event"),
        }
    }

    #[tokio::test]
    async fn test_observer_wrapper_includes_tool_initiator_id() {
        let (observer, mut rx) = ToolCallObserver::new(8);
        let wrapper = ObserverWrapper::new(observer, 42);

        let ctx = ToolCallContext::new("test_tool").with_task_context(0, "worker".into(), 1);
        let args = serde_json::json!({"param": "value"});

        // Call transform_args (should emit CallStarted)
        let result = wrapper.transform_args(args, &ctx);

        // Verify CallStarted was emitted
        let event = rx.recv().await.unwrap();
        match event {
            ToolEvent::CallStarted {
                tool_name,
                tool_call_id,
                tool_initiator_id,
                ..
            } => {
                assert_eq!(tool_name, "test_tool");
                assert!(tool_call_id.contains("task42"));
                assert_eq!(tool_initiator_id, "worker");
            }
            _ => panic!("Expected CallStarted event"),
        }

        // Call on_complete (should emit CallCompleted)
        wrapper
            .on_complete(&ctx, result.extracted.as_ref(), Ok("success"), 100)
            .await;

        let event = rx.recv().await.unwrap();
        match event {
            ToolEvent::CallCompleted {
                duration_ms,
                result,
                ..
            } => {
                assert_eq!(duration_ms, 100);
                assert!(result.is_success());
            }
            _ => panic!("Expected CallCompleted event"),
        }
    }

    #[tokio::test]
    async fn test_observer_wrapper_handles_errors() {
        let (observer, mut rx) = ToolCallObserver::new(8);
        let wrapper = ObserverWrapper::new(observer, 0);

        let ctx = ToolCallContext::new("failing_tool");
        let result = wrapper.transform_args(serde_json::json!({}), &ctx);

        // Skip CallStarted
        let _ = rx.recv().await.unwrap();

        // Call on_complete with error
        wrapper
            .on_complete(
                &ctx,
                result.extracted.as_ref(),
                Err("rate limit exceeded"),
                50,
            )
            .await;

        let event = rx.recv().await.unwrap();
        match event {
            ToolEvent::CallCompleted { result, .. } => {
                assert!(!result.is_success());
                assert_eq!(result.error_message(), Some("rate limit exceeded"));
            }
            _ => panic!("Expected CallCompleted event"),
        }
    }
}
