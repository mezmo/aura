//! Canonical parsing for boolean environment variables.
//!
//! Aura reads several boolean env vars (`AURA_CUSTOM_EVENTS`,
//! `AURA_PROMPT_JOURNAL`, `OTEL_RECORD_CONTENT`, …) at startup or on the
//! hot path. Historically each call site rolled its own truthy/falsy
//! check, which led to incompatible vocabularies — `AURA_ENRICH_REPLAN`
//! treated unknown values as truthy, while `OTEL_RECORD_CONTENT`
//! treated them as falsy, etc. This module is the single source of
//! truth so every flag understands the same vocabulary.
//!
//! # Vocabulary
//!
//! Mirrors clap's [`BoolishValueParser`] exactly so CLI flags
//! (`#[arg(env = ...)]`) and runtime reads (`bool_env`) accept an
//! identical set:
//!
//! - **Truthy** (case-insensitive, trimmed): `1`, `true`, `t`, `yes`, `y`, `on`
//! - **Falsy**  (case-insensitive, trimmed): `0`, `false`, `f`, `no`, `n`, `off`
//! - **Unrecognized** (including empty): logs a warning and falls back
//!   to the caller's default.
//!
//! [`BoolishValueParser`]: clap::builder::BoolishValueParser

/// Parse a string as a boolean using the canonical truthy/falsy vocabulary.
///
/// Returns `None` for unrecognized values (including empty strings) so
/// callers can decide how to surface the ambiguity. See the module
/// docs for the accepted vocabulary.
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Read a boolean environment variable using the canonical vocabulary.
///
/// Behavior:
/// - Var unset or empty → returns `default`.
/// - Recognized value (see [`parse_bool`]) → returns the parsed value.
/// - Unrecognized value → logs `warn!` and returns `default`. We log
///   instead of panicking so a typo in a deployment env file degrades
///   to documented default behavior rather than crashing the binary.
pub fn bool_env(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) if v.is_empty() => default,
        Ok(v) => match parse_bool(&v) {
            Some(b) => b,
            None => {
                tracing::warn!(
                    var = %name,
                    value = %v,
                    "Unrecognized boolean env var (expected 1/0, true/false, yes/no, on/off, t/f, y/n); using default={default}",
                );
                default
            }
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Locking around env mutation: `std::env::set_var` is process-global,
    // so concurrent tests touching the same var can race and read each
    // other's writes. Each test owns a unique var name to avoid the lock.

    #[test]
    fn parse_bool_truthy_canonical_set() {
        for v in [
            "1", "true", "t", "yes", "y", "on", "TRUE", "True", "Yes", "ON", "On", "T",
        ] {
            assert_eq!(parse_bool(v), Some(true), "expected truthy: {v:?}");
        }
    }

    #[test]
    fn parse_bool_falsy_canonical_set() {
        for v in [
            "0", "false", "f", "no", "n", "off", "FALSE", "False", "No", "OFF", "Off", "F",
        ] {
            assert_eq!(parse_bool(v), Some(false), "expected falsy: {v:?}");
        }
    }

    #[test]
    fn parse_bool_trims_whitespace() {
        assert_eq!(parse_bool("  true  "), Some(true));
        assert_eq!(parse_bool("\tfalse\n"), Some(false));
        assert_eq!(parse_bool(" 1 "), Some(true));
    }

    #[test]
    fn parse_bool_unrecognized_returns_none() {
        for v in [
            "", " ", "tru", "yess", "2", "-1", "enabled", "disabled", "banana",
        ] {
            assert_eq!(parse_bool(v), None, "expected None for: {v:?}");
        }
    }

    #[test]
    fn bool_env_unset_returns_default() {
        // Var name unique to this test so other tests can't race us.
        let name = "AURA_TEST_BOOL_ENV_UNSET";
        unsafe { std::env::remove_var(name) };
        assert!(bool_env(name, true));
        assert!(!bool_env(name, false));
    }

    #[test]
    fn bool_env_truthy_value_returns_true() {
        let name = "AURA_TEST_BOOL_ENV_TRUTHY";
        unsafe { std::env::set_var(name, "yes") };
        assert!(bool_env(name, false), "yes should beat default=false");
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn bool_env_falsy_value_returns_false() {
        let name = "AURA_TEST_BOOL_ENV_FALSY";
        unsafe { std::env::set_var(name, "off") };
        assert!(!bool_env(name, true), "off should beat default=true");
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn bool_env_unrecognized_returns_default() {
        let name = "AURA_TEST_BOOL_ENV_UNRECOGNIZED";
        unsafe { std::env::set_var(name, "banana") };
        assert!(
            bool_env(name, true),
            "unrecognized should fall back to default=true"
        );
        assert!(
            !bool_env(name, false),
            "unrecognized should fall back to default=false"
        );
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn bool_env_empty_returns_default() {
        let name = "AURA_TEST_BOOL_ENV_EMPTY";
        unsafe { std::env::set_var(name, "") };
        assert!(bool_env(name, true));
        assert!(!bool_env(name, false));
        unsafe { std::env::remove_var(name) };
    }
}
