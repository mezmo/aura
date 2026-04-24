//! Two-stage duplicate tool call guard for orchestration workers.
//!
//! Catches pathological looping behavior where a model calls the same tool
//! with identical arguments repeatedly despite receiving the same result.
//!
//! Uses `CallOutcome` for error-kind-aware counting:
//! - `Success` / `SchemaError` with identical output → counter increments
//! - `GeneralToolError` → counter unchanged (legitimate retries)
//!
//! Two escalation stages, both via annotation on the real tool output:
//! - `nudge_threshold` → appends `[DUPLICATE_CALL_GUIDANCE]`
//! - `block_threshold` → appends `[DUPLICATE_CALL_ABORT]` + sets escalation flag

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::mcp_response::CallOutcome;
use crate::tool_wrapper::{
    ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
};

const GUIDANCE_TEMPLATE: &str = include_str!("../prompts/duplicate_call_guidance.md");
const ABORT_TEMPLATE: &str = include_str!("../prompts/duplicate_call_abort.md");

/// Hash of `(tool_name, canonical_args)` identifying a unique invocation pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CallFingerprint(u64);

#[derive(Debug)]
struct CallState {
    consecutive_count: usize,
    last_output: String,
}

pub struct DuplicateCallGuard {
    nudge_threshold: usize,
    block_threshold: usize,
    escalation_flag: Arc<AtomicBool>,
    state: Mutex<HashMap<CallFingerprint, CallState>>,
}

impl DuplicateCallGuard {
    pub fn new(
        nudge_threshold: usize,
        block_threshold: usize,
        escalation_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            nudge_threshold,
            block_threshold,
            escalation_flag,
            state: Mutex::new(HashMap::new()),
        }
    }

    fn fingerprint(tool_name: &str, args: &Value) -> CallFingerprint {
        let canonical = canonicalize_args(args);
        let mut hasher = DefaultHasher::new();
        tool_name.hash(&mut hasher);
        canonical.hash(&mut hasher);
        CallFingerprint(hasher.finish())
    }

    fn render_template(template: &str, tool_name: &str, count: usize) -> String {
        template
            .replace("%%TOOL_NAME%%", tool_name)
            .replace("%%COUNT%%", &count.to_string())
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
        let fp = Self::fingerprint(&ctx.tool_name, &args);
        let extracted = serde_json::json!({
            "dup_guard_tool_name": ctx.tool_name,
            "dup_guard_args_hash": fp.0,
        });
        TransformArgsResult::with_extracted(args, extracted)
    }

    fn transform_output(
        &self,
        output: String,
        outcome: &CallOutcome,
        _ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> TransformOutputResult {
        let (tool_name, args_hash) = match extract_guard_data(extracted) {
            Some(v) => v,
            None => return TransformOutputResult::new(output),
        };

        if matches!(outcome, CallOutcome::GeneralToolError { .. }) {
            return TransformOutputResult::new(output);
        }

        let fp = CallFingerprint(args_hash);
        let mut state = self.state.lock().unwrap();

        if self.escalation_flag.load(Ordering::SeqCst) {
            return TransformOutputResult::new(output);
        }

        let call_state = state.entry(fp).or_insert_with(|| CallState {
            consecutive_count: 0,
            last_output: String::new(),
        });

        if call_state.last_output == output {
            call_state.consecutive_count += 1;
        } else {
            call_state.consecutive_count = 1;
            call_state.last_output = output.clone();
        }

        let count = call_state.consecutive_count;

        if count >= self.block_threshold {
            self.escalation_flag.store(true, Ordering::SeqCst);
            tracing::warn!(
                "DuplicateCallGuard: '{}' hit block threshold ({}/{})",
                tool_name,
                count,
                self.block_threshold
            );
            let annotation = Self::render_template(ABORT_TEMPLATE, &tool_name, count);
            return TransformOutputResult::new(format!("{output}\n\n{annotation}"));
        }

        if count >= self.nudge_threshold {
            tracing::info!(
                "DuplicateCallGuard: '{}' hit nudge threshold ({}/{})",
                tool_name,
                count,
                self.nudge_threshold
            );
            let annotation = Self::render_template(GUIDANCE_TEMPLATE, &tool_name, count);
            return TransformOutputResult::new(format!("{output}\n\n{annotation}"));
        }

        TransformOutputResult::new(output)
    }

    fn handle_error(
        &self,
        error: rig::tool::ToolError,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> rig::tool::ToolError {
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

    fn make_guard(nudge: usize, block: usize) -> (DuplicateCallGuard, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let guard = DuplicateCallGuard::new(nudge, block, flag.clone());
        (guard, flag)
    }

    fn call(
        guard: &DuplicateCallGuard,
        ctx: &ToolCallContext,
        args: &Value,
        output: &str,
        outcome: &CallOutcome,
    ) -> TransformOutputResult {
        let r = guard.transform_args(args.clone(), ctx);
        guard.transform_output(output.to_string(), outcome, ctx, r.extracted.as_ref())
    }

    fn success(s: &str) -> CallOutcome {
        CallOutcome::Success(s.to_string())
    }

    fn general_error(s: &str) -> CallOutcome {
        CallOutcome::GeneralToolError {
            content: s.to_string(),
            code: None,
        }
    }

    fn schema_error(s: &str) -> CallOutcome {
        CallOutcome::SchemaError {
            content: s.to_string(),
            code: -32602,
        }
    }

    #[test]
    fn test_canonicalize_args_strips_aura_reasoning() {
        let a = serde_json::json!({"number": 45, "_aura_reasoning": "reason A"});
        let b = serde_json::json!({"number": 45, "_aura_reasoning": "reason B"});
        let c = serde_json::json!({"number": 45});
        assert_eq!(canonicalize_args(&a), canonicalize_args(&b));
        assert_eq!(canonicalize_args(&a), canonicalize_args(&c));
    }

    #[test]
    fn test_canonicalize_args_sorts_keys() {
        let a = serde_json::json!({"b": 2, "a": 1});
        let b = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(canonicalize_args(&a), canonicalize_args(&b));
    }

    #[test]
    fn test_no_annotation_below_nudge() {
        let (guard, flag) = make_guard(3, 5);
        let ctx = ToolCallContext::new("mean");
        let args = serde_json::json!({"numbers": [1, 2, 3]});

        for _ in 0..2 {
            let r = call(&guard, &ctx, &args, "2.0", &success("2.0"));
            assert!(!r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
            assert!(!r.output.contains("[DUPLICATE_CALL_ABORT]"));
        }
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_nudge_at_threshold() {
        let (guard, flag) = make_guard(3, 5);
        let ctx = ToolCallContext::new("mean");
        let args = serde_json::json!({"numbers": [1, 2, 3]});

        for _ in 0..2 {
            call(&guard, &ctx, &args, "2.0", &success("2.0"));
        }

        let r = call(&guard, &ctx, &args, "2.0", &success("2.0"));
        assert!(r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
        assert!(r.output.starts_with("2.0"));
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_block_at_threshold_sets_flag() {
        let (guard, flag) = make_guard(3, 5);
        let ctx = ToolCallContext::new("mean");
        let args = serde_json::json!({"numbers": [1, 2, 3]});

        for _ in 0..4 {
            call(&guard, &ctx, &args, "2.0", &success("2.0"));
        }

        let r = call(&guard, &ctx, &args, "2.0", &success("2.0"));
        assert!(r.output.contains("[DUPLICATE_CALL_ABORT]"));
        assert!(r.output.starts_with("2.0"));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_general_error_does_not_increment() {
        let (guard, flag) = make_guard(2, 4);
        let ctx = ToolCallContext::new("api");
        let args = serde_json::json!({"q": "test"});

        call(&guard, &ctx, &args, "timeout", &success("timeout"));

        for _ in 0..5 {
            let r = call(&guard, &ctx, &args, "timeout", &general_error("timeout"));
            assert!(!r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
            assert!(!r.output.contains("[DUPLICATE_CALL_ABORT]"));
        }
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_schema_error_increments() {
        let (guard, flag) = make_guard(2, 4);
        let ctx = ToolCallContext::new("api");
        let args = serde_json::json!({"bad": "field"});

        call(
            &guard,
            &ctx,
            &args,
            "missing param",
            &schema_error("missing param"),
        );

        let r = call(
            &guard,
            &ctx,
            &args,
            "missing param",
            &schema_error("missing param"),
        );
        assert!(r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_different_result_resets() {
        let (guard, _flag) = make_guard(2, 4);
        let ctx = ToolCallContext::new("get_time");
        let args = serde_json::json!({"tz": "UTC"});

        call(&guard, &ctx, &args, "12:00", &success("12:00"));
        call(&guard, &ctx, &args, "12:01", &success("12:01"));

        let r = call(&guard, &ctx, &args, "12:01", &success("12:01"));
        assert!(r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
    }

    #[test]
    fn test_different_tool_resets() {
        let (guard, _flag) = make_guard(2, 4);
        let args = serde_json::json!({"x": 1});
        let ctx_a = ToolCallContext::new("tool_a");
        let ctx_b = ToolCallContext::new("tool_b");

        call(&guard, &ctx_a, &args, "1", &success("1"));
        call(&guard, &ctx_a, &args, "1", &success("1"));

        let r = call(&guard, &ctx_b, &args, "1", &success("1"));
        assert!(!r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
    }

    #[test]
    fn test_flag_monotonic_after_block() {
        let (guard, flag) = make_guard(2, 3);
        let ctx = ToolCallContext::new("t");
        let args = serde_json::json!({"x": 1});

        for _ in 0..3 {
            call(&guard, &ctx, &args, "r", &success("r"));
        }
        assert!(flag.load(Ordering::SeqCst));

        let r = call(&guard, &ctx, &args, "r", &success("r"));
        assert!(!r.output.contains("[DUPLICATE_CALL_ABORT]"));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_real_output_preserved() {
        let (guard, _flag) = make_guard(2, 4);
        let ctx = ToolCallContext::new("calc");
        let args = serde_json::json!({"x": 5});

        call(&guard, &ctx, &args, "25", &success("25"));
        let r = call(&guard, &ctx, &args, "25", &success("25"));
        assert!(r.output.starts_with("25"));
        assert!(r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
    }

    #[test]
    fn test_nudge_then_block_progression() {
        let (guard, flag) = make_guard(3, 5);
        let ctx = ToolCallContext::new("calc");
        let args = serde_json::json!({"x": 5});

        // Calls 1-2: below nudge_threshold=3
        for i in 1..=2 {
            let r = call(&guard, &ctx, &args, "25", &success("25"));
            assert!(
                !r.output.contains("[DUPLICATE_CALL"),
                "unexpected annotation on call {i}"
            );
        }

        // Calls 3-4: at/above nudge, below block
        for _ in 3..=4 {
            let r = call(&guard, &ctx, &args, "25", &success("25"));
            assert!(r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));
            assert!(!r.output.contains("[DUPLICATE_CALL_ABORT]"));
        }
        assert!(!flag.load(Ordering::SeqCst));

        // Call 5: block + flag
        let r = call(&guard, &ctx, &args, "25", &success("25"));
        assert!(r.output.contains("[DUPLICATE_CALL_ABORT]"));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_ping_pong_tracked_independently() {
        let (guard, flag) = make_guard(3, 5);
        let args_a = serde_json::json!({"x": 1});
        let args_b = serde_json::json!({"x": 2});
        let ctx = ToolCallContext::new("calc");

        // Alternate: A, B, A, B — each pair's counter is independent
        for _ in 0..2 {
            call(&guard, &ctx, &args_a, "1", &success("1"));
            call(&guard, &ctx, &args_b, "2", &success("2"));
        }

        // A at count=3 → nudge
        let r = call(&guard, &ctx, &args_a, "1", &success("1"));
        assert!(r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));

        // B at count=3 → nudge
        let r = call(&guard, &ctx, &args_b, "2", &success("2"));
        assert!(r.output.contains("[DUPLICATE_CALL_GUIDANCE]"));

        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_extract_guard_data_from_composed_array() {
        let extracted = serde_json::json!([
            {"observer_tool_call_id": "task0_mean_1"},
            {"dup_guard_tool_name": "mean", "dup_guard_args_hash": 12345},
            {"persistence_key": "abc"}
        ]);

        let (name, hash) = extract_guard_data(Some(&extracted)).unwrap();
        assert_eq!(name, "mean");
        assert_eq!(hash, 12345);
    }

    #[test]
    fn test_extract_guard_data_direct() {
        let extracted =
            serde_json::json!({"dup_guard_tool_name": "sin", "dup_guard_args_hash": 99});

        let (name, hash) = extract_guard_data(Some(&extracted)).unwrap();
        assert_eq!(name, "sin");
        assert_eq!(hash, 99);
    }
}
