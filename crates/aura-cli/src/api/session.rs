/// Header name used to communicate the chat session identifier.
pub const CHAT_SESSION_HEADER: &str = "x-chat-session-id";

/// Suffix appended to the resolved session ID for summary/title-generation calls.
pub const SUMMARY_SUFFIX: &str = "-summary";

/// Env var that opts in to LLM-based final-response title generation.
pub const FINAL_RESPONSE_SUMMARY_ENV: &str = "AURA_ENABLE_FINAL_RESPONSE_SUMMARY";

/// Whether to call the LLM for a one-line title summarizing each final response,
/// based on the `AURA_ENABLE_FINAL_RESPONSE_SUMMARY` env var.
///
/// Disabled by default — enabled only when the env var is set to `true` or `1`
/// (case-insensitive). Callers should normally read `AppConfig.enable_final_response_summary`
/// instead, which resolves the full CLI > file > env > default precedence; this
/// helper is the env-var leaf used by that resolver.
pub fn is_final_response_summary_enabled() -> bool {
    match std::env::var(FINAL_RESPONSE_SUMMARY_ENV) {
        Ok(val) => {
            let v = val.trim();
            v.eq_ignore_ascii_case("true") || v == "1"
        }
        Err(_) => false,
    }
}

/// Split `text` into a `(summary, body)` pair: the first non-empty line becomes
/// the summary, and the body is the remainder (preserving its internal whitespace).
///
/// Used as the fallback when [`is_final_response_summary_enabled`] is false so the
/// bullet header still carries meaningful content without the body re-rendering it.
///
/// - Leading blank lines in `text` are skipped before picking the summary line.
/// - If `text` has no newline, the whole string becomes the summary and the body is empty.
/// - If `text` is empty (or whitespace-only), both halves are empty.
pub fn split_first_line_summary(text: &str) -> (String, String) {
    let trimmed_start = text.trim_start_matches(['\n', '\r']);
    match trimmed_start.split_once('\n') {
        Some((first, rest)) => (first.trim_end().to_string(), rest.to_string()),
        None => (trimmed_start.trim_end().to_string(), String::new()),
    }
}

/// Kind of request for which the session ID is being resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Chat,
    Summary,
}

/// Locate a header in `extra_headers` by case-insensitive name.
pub fn find_header_value<'a>(extra_headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    extra_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Resolve the `x-chat-session-id` value to use for a request.
///
/// Precedence:
/// 1. If `extra_headers` (from `AURA_EXTRA_HEADERS`) sets `x-chat-session-id`, use it as the base.
/// 2. Otherwise, use `conversation_uuid` as the base.
///
/// For `SessionKind::Summary`, append `-summary` to the resolved base so that
/// title-generation calls correlate with their parent chat session but don't
/// share the same logical session.
pub fn resolve_chat_session_id(
    extra_headers: &[(String, String)],
    conversation_uuid: &str,
    kind: SessionKind,
) -> String {
    let base = find_header_value(extra_headers, CHAT_SESSION_HEADER)
        .map(str::to_string)
        .unwrap_or_else(|| conversation_uuid.to_string());

    match kind {
        SessionKind::Chat => base,
        SessionKind::Summary => format!("{base}{SUMMARY_SUFFIX}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn falls_back_to_conversation_uuid_when_header_missing() {
        let h = headers(&[("authorization", "Bearer foo")]);
        assert_eq!(
            resolve_chat_session_id(&h, "conv-123", SessionKind::Chat),
            "conv-123"
        );
    }

    #[test]
    fn uses_header_value_when_present() {
        let h = headers(&[("x-chat-session-id", "override-abc")]);
        assert_eq!(
            resolve_chat_session_id(&h, "conv-123", SessionKind::Chat),
            "override-abc"
        );
    }

    #[test]
    fn header_match_is_case_insensitive() {
        let h = headers(&[("X-Chat-Session-Id", "override-abc")]);
        assert_eq!(
            resolve_chat_session_id(&h, "conv-123", SessionKind::Chat),
            "override-abc"
        );
    }

    #[test]
    fn summary_suffix_appended_to_uuid_fallback() {
        let h = headers(&[]);
        assert_eq!(
            resolve_chat_session_id(&h, "conv-123", SessionKind::Summary),
            "conv-123-summary"
        );
    }

    #[test]
    fn summary_suffix_appended_to_header_override() {
        let h = headers(&[("x-chat-session-id", "override-abc")]);
        assert_eq!(
            resolve_chat_session_id(&h, "conv-123", SessionKind::Summary),
            "override-abc-summary"
        );
    }

    #[test]
    fn split_first_line_with_newline() {
        let (s, rest) = split_first_line_summary("Title goes here\nbody line 1\nbody line 2");
        assert_eq!(s, "Title goes here");
        assert_eq!(rest, "body line 1\nbody line 2");
    }

    #[test]
    fn split_first_line_no_newline() {
        let (s, rest) = split_first_line_summary("just one line");
        assert_eq!(s, "just one line");
        assert_eq!(rest, "");
    }

    #[test]
    fn split_first_line_skips_leading_blank_lines() {
        let (s, rest) = split_first_line_summary("\n\nTitle\nbody");
        assert_eq!(s, "Title");
        assert_eq!(rest, "body");
    }

    #[test]
    fn split_first_line_strips_trailing_cr() {
        let (s, rest) = split_first_line_summary("Title\r\nbody");
        assert_eq!(s, "Title");
        assert_eq!(rest, "body");
    }

    #[test]
    fn split_first_line_empty_input() {
        let (s, rest) = split_first_line_summary("");
        assert_eq!(s, "");
        assert_eq!(rest, "");
    }

    #[test]
    fn find_header_value_case_insensitive() {
        let h = headers(&[("X-Chat-Session-Id", "v")]);
        assert_eq!(find_header_value(&h, "x-chat-session-id"), Some("v"));
        assert_eq!(find_header_value(&h, "X-CHAT-SESSION-ID"), Some("v"));
        assert_eq!(find_header_value(&h, "missing"), None);
    }

    /// Tests for `is_final_response_summary_enabled` mutate process env and
    /// must not run concurrently with each other.
    mod summary_env {
        use super::super::*;
        use std::sync::Mutex;

        static ENV_LOCK: Mutex<()> = Mutex::new(());

        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: tests in this submodule serialize on ENV_LOCK.
                unsafe { std::env::remove_var(FINAL_RESPONSE_SUMMARY_ENV) };
            }
        }

        fn set(value: &str) -> EnvGuard {
            // SAFETY: tests in this submodule serialize on ENV_LOCK.
            unsafe { std::env::set_var(FINAL_RESPONSE_SUMMARY_ENV, value) };
            EnvGuard
        }

        fn unset() -> EnvGuard {
            // SAFETY: tests in this submodule serialize on ENV_LOCK.
            unsafe { std::env::remove_var(FINAL_RESPONSE_SUMMARY_ENV) };
            EnvGuard
        }

        #[test]
        fn disabled_by_default() {
            let _lock = ENV_LOCK.lock().unwrap();
            let _g = unset();
            assert!(!is_final_response_summary_enabled());
        }

        #[test]
        fn enabled_for_true() {
            let _lock = ENV_LOCK.lock().unwrap();
            let _g = set("true");
            assert!(is_final_response_summary_enabled());
        }

        #[test]
        fn enabled_for_one() {
            let _lock = ENV_LOCK.lock().unwrap();
            let _g = set("1");
            assert!(is_final_response_summary_enabled());
        }

        #[test]
        fn case_insensitive_true() {
            let _lock = ENV_LOCK.lock().unwrap();
            let _g = set("TRUE");
            assert!(is_final_response_summary_enabled());
        }

        #[test]
        fn rejects_other_values() {
            let _lock = ENV_LOCK.lock().unwrap();
            for val in &["", "0", "false", "yes", "2", "on"] {
                let _g = set(val);
                assert!(
                    !is_final_response_summary_enabled(),
                    "value {val:?} should not enable the flag"
                );
            }
        }
    }
}
