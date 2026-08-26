//! Scrub nondeterministic values from captured SSE events and store state.

use std::path::Path;

use regex::Regex;
use serde_json::Value;

/// Replace run/session/approval IDs, timestamps, durations, host/port addresses,
/// filesystem paths, and the `aura_version` release string with stable
/// placeholders.
pub fn scrub_nondeterminism(value: &mut Value, memory_dir: &Path) {
    let memory_dir_str = memory_dir.to_string_lossy().to_string();
    let cs_re = Regex::new(r"cs_[0-9a-fA-F]{16,}").expect("cs regex compiles");
    let uuid_re = Regex::new(
        r"(^|[^0-9A-Za-z-])([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})([^0-9A-Za-z-]|$)",
    )
    .expect("uuid regex compiles");
    scrub_value(value, &memory_dir_str, &cs_re, &uuid_re);
}

fn scrub_value(value: &mut Value, memory_dir: &str, cs_re: &Regex, uuid_re: &Regex) {
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
                } else if key == "aura_version" {
                    *val = Value::String("<version>".to_string());
                } else if key == "latency_ms" {
                    *val = Value::String("<ms>".to_string());
                } else if key == "base_url" || key == "url" {
                    if let Value::String(s) = val {
                        *s = scrub_url(s);
                    }
                } else if let Value::String(s) = val {
                    *s = scrub_path_string(s, memory_dir, cs_re, uuid_re);
                } else {
                    scrub_value(val, memory_dir, cs_re, uuid_re);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_value(item, memory_dir, cs_re, uuid_re);
            }
        }
        Value::String(s) => {
            *s = scrub_path_string(s, memory_dir, cs_re, uuid_re);
        }
        _ => {}
    }
}

fn scrub_path_string(s: &str, memory_dir: &str, cs_re: &Regex, uuid_re: &Regex) -> String {
    let mut out = s.to_string();
    if out.contains(memory_dir) {
        out = out.replace(memory_dir, "<memory_dir>");
    }
    out = cs_re.replace_all(&out, "<session>").to_string();
    out = uuid_re.replace_all(&out, "${1}<run>${3}").to_string();
    out
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
            | "tool_id"
            | "trace_id"
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
