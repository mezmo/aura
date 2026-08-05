//! Scrub nondeterministic values from captured SSE events and store state.

use regex::Regex;
use serde_json::Value;

/// Replace run/session/approval IDs, timestamps, durations, and host/port
/// addresses with stable placeholders.
pub fn scrub_nondeterminism(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_id_key(key) {
                    if let Value::String(s) = val {
                        *s = placeholder_for(s);
                    }
                } else if is_timestamp_key(key) {
                    if let Value::String(_) | Value::Number(_) = val {
                        *val = Value::String("<timestamp>".to_string());
                    }
                } else if key == "duration_ms" || key == "elapsed_ms" {
                    *val = Value::String("<duration>".to_string());
                } else if key == "base_url" || key == "url" {
                    if let Value::String(s) = val {
                        *s = scrub_url(s);
                    }
                } else {
                    scrub_nondeterminism(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_nondeterminism(item);
            }
        }
        _ => {}
    }
}
fn is_id_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "run_id"
            | "session_id"
            | "approval_id"
            | "decision_id"
            | "request_id"
            | "tool_call_id"
            | "orchestrator_id"
            | "worker_id"
            | "task_id"
            | "message_id"
            | "correlation_id"
    )
}

fn is_timestamp_key(key: &str) -> bool {
    matches!(
        key,
        "created_at" | "timestamp" | "started_at" | "finished_at" | "created" | "expires_at"
    )
}

fn placeholder_for(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    if uuid::Uuid::parse_str(s).is_ok() {
        return "<uuid>".to_string();
    }
    if looks_like_ulid(s) {
        return "<ulid>".to_string();
    }
    if s.starts_with("run-") || s.starts_with("session-") {
        return "<id>".to_string();
    }
    "<id>".to_string()
}

fn looks_like_ulid(s: &str) -> bool {
    s.len() == 26 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn scrub_url(s: &str) -> String {
    let re = Regex::new(r"https?://[^/]+").expect("url regex compiles");
    re.replace_all(s, "http://<host>").to_string()
}
