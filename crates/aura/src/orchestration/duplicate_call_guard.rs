//! Duplicate tool call guard for orchestration workers.
//!
//! Catches pathological looping behavior where a model calls the same tool
//! with identical arguments repeatedly despite receiving the same result.
//! Common with smaller/quantized models (e.g., Qwen 3.5 Q4_K_M) that lack
//! strong internalized "stop after result" behavior.
//!
//! The guard tracks consecutive identical calls per (tool_name, args) pair.
//! After `max_duplicates` consecutive identical successful results, it rejects
//! the call with a nudge message directing the model to emit its final answer.
//!
//! Resets on:
//! - Different tool or different args (new work)
//! - Same args but different result (transient data)
//! - Tool error (retries after errors are healthy)

use async_trait::async_trait;
use rig::tool::ToolError;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crate::tool_wrapper::{
    ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
};

/// State for tracking consecutive duplicate calls.
#[derive(Debug)]
struct CallState {
    /// Number of consecutive times this (tool, args) pair returned the same result.
    consecutive_count: usize,
    /// The last successful result for comparison.
    last_result: String,
}

/// Guards against infinite tool-call loops by rejecting consecutive duplicate calls.
///
/// When a model calls the same tool with identical arguments and receives the same
/// result `max_duplicates` times in a row, subsequent calls are rejected with a
/// nudge message that directs the model to emit its final answer.
///
/// # Design
///
/// - Fresh instance per worker (no cross-task state leakage)
/// - Only tracks the most recent (tool, args) pair — different tool/args resets
/// - Resets on errors (retries after transient failures are healthy)
/// - Resets when same args return a different result (transient data changes)
pub struct DuplicateCallGuard {
    max_duplicates: usize,
    // Key: (tool_name, args_hash). Only one entry tracked at a time (last call).
    state: Mutex<Option<(String, u64, CallState)>>,
}

impl DuplicateCallGuard {
    /// Create a new guard with the specified duplicate threshold.
    ///
    /// `max_duplicates` is the number of consecutive identical successful calls
    /// allowed before rejection. Default: 2.
    pub fn new(max_duplicates: usize) -> Self {
        Self {
            max_duplicates,
            state: Mutex::new(None),
        }
    }

    /// Compute a deterministic hash for canonical (tool_name, sorted_args).
    fn hash_call(tool_name: &str, args: &Value) -> u64 {
        let canonical = canonicalize_args(args);
        let mut hasher = DefaultHasher::new();
        tool_name.hash(&mut hasher);
        canonical.hash(&mut hasher);
        hasher.finish()
    }
}

/// Canonicalize JSON args by sorting object keys for stable hashing.
/// Strips `_aura_reasoning` since it varies per call but doesn't affect tool behavior.
fn canonicalize_args(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut pairs: Vec<_> = map
                .iter()
                .filter(|(k, _)| k.as_str() != "_aura_reasoning")
                .collect();
            pairs.sort_by_key(|(k, _)| *k);
            let inner: Vec<String> = pairs
                .into_iter()
                .map(|(k, v)| format!("{}:{}", k, canonicalize_args(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonicalize_args).collect();
            format!("[{}]", inner.join(","))
        }
        _ => value.to_string(),
    }
}

#[async_trait]
impl ToolWrapper for DuplicateCallGuard {
    fn transform_args(&self, args: Value, ctx: &ToolCallContext) -> TransformArgsResult {
        let hash = Self::hash_call(&ctx.tool_name, &args);
        // Store the hash and tool_name in extracted for validate_args and transform_output
        let extracted = serde_json::json!({
            "dup_guard_tool_name": ctx.tool_name,
            "dup_guard_args_hash": hash,
        });
        TransformArgsResult::with_extracted(args, extracted)
    }

    fn validate_args(
        &self,
        _args: &Value,
        extracted: Option<&Value>,
        _ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        let (tool_name, args_hash) = match extract_guard_data(extracted) {
            Some(v) => v,
            None => return Ok(()),
        };

        let state = self.state.lock().unwrap();
        if let Some((ref tracked_tool, tracked_hash, ref call_state)) = *state
            && tracked_tool == &tool_name
            && tracked_hash == args_hash
            && call_state.consecutive_count >= self.max_duplicates
        {
            let cached = &call_state.last_result;
            let count = call_state.consecutive_count;
            return Err(ToolError::ToolCallError(
                format!(
                    "[DUPLICATE TOOL CALL] You have called '{}' with identical arguments {} times \
                     and received the same result: {}. \
                     Write your final answer now. Do not call this tool again with the same arguments.",
                    tool_name, count, cached
                )
                .into(),
            ));
        }

        Ok(())
    }

    fn transform_output(
        &self,
        output: String,
        _ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> TransformOutputResult {
        let (tool_name, args_hash) = match extract_guard_data(extracted) {
            Some(v) => v,
            None => return TransformOutputResult::new(output),
        };

        let mut state = self.state.lock().unwrap();

        let is_match = state
            .as_ref()
            .is_some_and(|(t, h, _)| t == &tool_name && *h == args_hash);

        if is_match {
            let (_, _, call_state) = state.as_mut().unwrap();
            if call_state.last_result == output {
                // Same tool + same args + same result → increment
                call_state.consecutive_count += 1;
                tracing::debug!(
                    "DuplicateCallGuard: '{}' identical call #{} (hash={})",
                    tool_name,
                    call_state.consecutive_count,
                    args_hash
                );
            } else {
                // Same args but different result → reset (transient data)
                call_state.consecutive_count = 1;
                call_state.last_result = output.clone();
            }
        } else {
            // New tool or new args → fresh tracking
            *state = Some((
                tool_name,
                args_hash,
                CallState {
                    consecutive_count: 1,
                    last_result: output.clone(),
                },
            ));
        }

        TransformOutputResult::new(output)
    }

    fn handle_error(
        &self,
        error: ToolError,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> ToolError {
        // Only reset on non-guard errors — retries after transient failures are healthy,
        // but we must not reset when our own rejection is routed back through handle_error
        let error_msg = error.to_string();
        if !error_msg.contains("[DUPLICATE TOOL CALL]") {
            let mut state = self.state.lock().unwrap();
            *state = None;
        }
        error
    }
}

/// Extract guard data from the extracted value, handling both direct and ComposedWrapper array formats.
fn extract_guard_data(extracted: Option<&Value>) -> Option<(String, u64)> {
    extracted.and_then(|v| {
        // Handle ComposedWrapper array format
        let obj = if let Some(arr) = v.as_array() {
            arr.iter()
                .find(|item| item.get("dup_guard_tool_name").is_some())
        } else if v.get("dup_guard_tool_name").is_some() {
            Some(v)
        } else {
            None
        };

        obj.and_then(|o| {
            let tool_name = o.get("dup_guard_tool_name")?.as_str()?.to_string();
            let args_hash = o.get("dup_guard_args_hash")?.as_u64()?;
            Some((tool_name, args_hash))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonicalize_args_strips_aura_reasoning() {
        let a = serde_json::json!({"number": 45, "_aura_reasoning": "Converting 45 degrees"});
        let b = serde_json::json!({"number": 45, "_aura_reasoning": "Different reasoning text"});
        let c = serde_json::json!({"number": 45});
        assert_eq!(canonicalize_args(&a), canonicalize_args(&b));
        assert_eq!(canonicalize_args(&a), canonicalize_args(&c));
    }

    #[test]
    fn test_guard_treats_different_reasoning_as_same_call() {
        let guard = DuplicateCallGuard::new(2);
        let ctx = ToolCallContext::new("degreesToRadians");

        // Call 1 with reasoning A
        let args1 = serde_json::json!({"number": 45, "_aura_reasoning": "Converting 45 degrees"});
        let r = guard.transform_args(args1, &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("0.785".to_string(), &ctx, r.extracted.as_ref());

        // Call 2 with different reasoning but same functional args
        let args2 = serde_json::json!({"number": 45, "_aura_reasoning": "Need radians for sin"});
        let r = guard.transform_args(args2, &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("0.785".to_string(), &ctx, r.extracted.as_ref());

        // Call 3 — should be REJECTED
        let args3 = serde_json::json!({"number": 45, "_aura_reasoning": "Yet another reason"});
        let r = guard.transform_args(args3, &ctx);
        assert!(
            guard
                .validate_args(&r.args, r.extracted.as_ref(), &ctx)
                .is_err()
        );
    }

    #[test]
    fn test_canonicalize_args_sorts_keys() {
        let a = serde_json::json!({"b": 2, "a": 1});
        let b = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(canonicalize_args(&a), canonicalize_args(&b));
    }

    #[test]
    fn test_canonicalize_args_nested() {
        let v = serde_json::json!({"z": [1, {"b": 2, "a": 1}], "a": "hello"});
        let canonical = canonicalize_args(&v);
        assert!(canonical.contains("a:\"hello\""));
        assert!(canonical.contains("{a:1,b:2}"));
    }

    #[test]
    fn test_guard_allows_first_calls() {
        let guard = DuplicateCallGuard::new(2);
        let ctx = ToolCallContext::new("mean");
        let args = serde_json::json!({"numbers": [1, 2, 3]});

        let result = guard.transform_args(args, &ctx);
        assert!(
            guard
                .validate_args(&result.args, result.extracted.as_ref(), &ctx)
                .is_ok()
        );

        // Simulate successful output
        guard.transform_output("2.0".to_string(), &ctx, result.extracted.as_ref());
    }

    #[test]
    fn test_guard_allows_up_to_max_duplicates() {
        let guard = DuplicateCallGuard::new(2);
        let ctx = ToolCallContext::new("mean");
        let args = serde_json::json!({"numbers": [1, 2, 3]});

        // Call 1: allowed, output recorded (count=1)
        let r1 = guard.transform_args(args.clone(), &ctx);
        assert!(
            guard
                .validate_args(&r1.args, r1.extracted.as_ref(), &ctx)
                .is_ok()
        );
        guard.transform_output("2.0".to_string(), &ctx, r1.extracted.as_ref());

        // Call 2: allowed (count becomes 2 = max_duplicates)
        let r2 = guard.transform_args(args.clone(), &ctx);
        assert!(
            guard
                .validate_args(&r2.args, r2.extracted.as_ref(), &ctx)
                .is_ok()
        );
        guard.transform_output("2.0".to_string(), &ctx, r2.extracted.as_ref());

        // Call 3: REJECTED (count=2 >= max_duplicates=2)
        let r3 = guard.transform_args(args.clone(), &ctx);
        let result = guard.validate_args(&r3.args, r3.extracted.as_ref(), &ctx);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("DUPLICATE TOOL CALL"));
        assert!(err_msg.contains("2.0"));
    }

    #[test]
    fn test_guard_resets_on_different_args() {
        let guard = DuplicateCallGuard::new(2);
        let ctx = ToolCallContext::new("mean");
        let args1 = serde_json::json!({"numbers": [1, 2, 3]});
        let args2 = serde_json::json!({"numbers": [4, 5, 6]});

        // Fill up with args1
        let r = guard.transform_args(args1.clone(), &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("2.0".to_string(), &ctx, r.extracted.as_ref());

        let r = guard.transform_args(args1.clone(), &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("2.0".to_string(), &ctx, r.extracted.as_ref());

        // Different args → resets, allowed
        let r = guard.transform_args(args2, &ctx);
        assert!(
            guard
                .validate_args(&r.args, r.extracted.as_ref(), &ctx)
                .is_ok()
        );
    }

    #[test]
    fn test_guard_resets_on_different_result() {
        let guard = DuplicateCallGuard::new(2);
        let ctx = ToolCallContext::new("get_time");
        let args = serde_json::json!({"tz": "UTC"});

        // Call 1: result "12:00"
        let r = guard.transform_args(args.clone(), &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("12:00".to_string(), &ctx, r.extracted.as_ref());

        // Call 2: same args, different result → resets
        let r = guard.transform_args(args.clone(), &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("12:01".to_string(), &ctx, r.extracted.as_ref());

        // Call 3: same args, same new result → count=2
        let r = guard.transform_args(args.clone(), &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("12:01".to_string(), &ctx, r.extracted.as_ref());

        // Call 4: REJECTED
        let r = guard.transform_args(args.clone(), &ctx);
        assert!(
            guard
                .validate_args(&r.args, r.extracted.as_ref(), &ctx)
                .is_err()
        );
    }

    #[test]
    fn test_guard_resets_on_error() {
        let guard = DuplicateCallGuard::new(2);
        let ctx = ToolCallContext::new("mean");
        let args = serde_json::json!({"numbers": [1, 2, 3]});

        // Fill up
        let r = guard.transform_args(args.clone(), &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("2.0".to_string(), &ctx, r.extracted.as_ref());

        let r = guard.transform_args(args.clone(), &ctx);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx)
            .ok();
        guard.transform_output("2.0".to_string(), &ctx, r.extracted.as_ref());

        // Error resets
        guard.handle_error(ToolError::ToolCallError("timeout".into()), &ctx, None);

        // Now allowed again
        let r = guard.transform_args(args, &ctx);
        assert!(
            guard
                .validate_args(&r.args, r.extracted.as_ref(), &ctx)
                .is_ok()
        );
    }

    #[test]
    fn test_guard_resets_on_different_tool() {
        let guard = DuplicateCallGuard::new(2);
        let args = serde_json::json!({"x": 1});

        let ctx_a = ToolCallContext::new("tool_a");
        let ctx_b = ToolCallContext::new("tool_b");

        // Fill up tool_a
        let r = guard.transform_args(args.clone(), &ctx_a);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx_a)
            .ok();
        guard.transform_output("1".to_string(), &ctx_a, r.extracted.as_ref());

        let r = guard.transform_args(args.clone(), &ctx_a);
        guard
            .validate_args(&r.args, r.extracted.as_ref(), &ctx_a)
            .ok();
        guard.transform_output("1".to_string(), &ctx_a, r.extracted.as_ref());

        // Different tool → resets
        let r = guard.transform_args(args, &ctx_b);
        assert!(
            guard
                .validate_args(&r.args, r.extracted.as_ref(), &ctx_b)
                .is_ok()
        );
    }

    #[test]
    fn test_extract_guard_data_from_composed_array() {
        let extracted = serde_json::json!([
            {"observer_tool_call_id": "task0_mean_1"},
            {"dup_guard_tool_name": "mean", "dup_guard_args_hash": 12345},
            {"persistence_key": "abc"}
        ]);

        let result = extract_guard_data(Some(&extracted));
        assert!(result.is_some());
        let (name, hash) = result.unwrap();
        assert_eq!(name, "mean");
        assert_eq!(hash, 12345);
    }

    #[test]
    fn test_extract_guard_data_direct() {
        let extracted =
            serde_json::json!({"dup_guard_tool_name": "sin", "dup_guard_args_hash": 99});

        let result = extract_guard_data(Some(&extracted));
        assert!(result.is_some());
        let (name, hash) = result.unwrap();
        assert_eq!(name, "sin");
        assert_eq!(hash, 99);
    }
}
