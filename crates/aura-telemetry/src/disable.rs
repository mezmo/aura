//! Kill-switch decision tree.
//!
//! Evaluated exactly once at telemetry init. First match wins; subsequent
//! checks are skipped. The order is intentional and load-bearing — see
//! `docs/telemetry.md` for the user-facing precedence table.

/// Reason a telemetry run is disabled. `None` (returned by
/// [`decide_disabled`]) means telemetry is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisableReason {
    /// `DO_NOT_TRACK` honored (industry-standard cross-tool opt-out).
    DoNotTrack,
    /// Aura-specific opt-out via `AURA_TELEMETRY_DISABLED`.
    AuraDisabled,
    /// Detected CI environment. The string names the env var that matched.
    Ci(&'static str),
    /// Running under cargo test (CARGO_TARGET_TMPDIR or RUST_TEST_THREADS).
    CargoTest,
    /// Config explicitly set `[telemetry] enabled = false`.
    ConfigDisabled,
}

/// Pluggable env provider so tests don't touch the process env (which
/// leaks between parallel `cargo test`s).
pub trait EnvProvider {
    fn var(&self, key: &str) -> Option<String>;
}

/// Read-from-the-real-environment provider. Default outside tests.
pub struct SystemEnv;

impl EnvProvider for SystemEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

const CI_ENV_VARS: &[&str] = &[
    "CI",
    "GITHUB_ACTIONS",
    "BUILDKITE",
    "JENKINS_URL",
    "CIRCLECI",
    "GITLAB_CI",
    "TF_BUILD",
    "TEAMCITY_VERSION",
    "TRAVIS",
];

fn is_truthy(v: &Option<String>) -> bool {
    match v {
        None => false,
        Some(s) => !s.is_empty() && s != "0",
    }
}

/// Decide whether telemetry is disabled based on environment.
///
/// Precedence: `DO_NOT_TRACK` → `AURA_TELEMETRY_DISABLED` → CI envs →
/// cargo-test markers → `AURA_TELEMETRY_ENABLED=false`. Config-from-TOML
/// disable is layered separately by the caller after this returns.
///
/// Returns `Some(reason)` if disabled, `None` if active.
pub fn decide_disabled(env: &dyn EnvProvider) -> Option<DisableReason> {
    if is_truthy(&env.var("DO_NOT_TRACK")) {
        return Some(DisableReason::DoNotTrack);
    }
    if is_truthy(&env.var("AURA_TELEMETRY_DISABLED")) {
        return Some(DisableReason::AuraDisabled);
    }
    for name in CI_ENV_VARS {
        if is_truthy(&env.var(name)) {
            return Some(DisableReason::Ci(name));
        }
    }
    if env.var("CARGO_TARGET_TMPDIR").is_some() || env.var("RUST_TEST_THREADS").is_some() {
        return Some(DisableReason::CargoTest);
    }
    if let Some(v) = env.var("AURA_TELEMETRY_ENABLED") {
        if v == "false" || v == "0" {
            return Some(DisableReason::ConfigDisabled);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Test double — gives every test its own isolated env map.
    #[derive(Default)]
    struct MockEnv(HashMap<String, String>);

    impl MockEnv {
        fn new() -> Self {
            Self(HashMap::new())
        }
        fn set(mut self, key: &str, value: &str) -> Self {
            self.0.insert(key.to_string(), value.to_string());
            self
        }
    }

    impl EnvProvider for MockEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn empty_env_means_active() {
        assert_eq!(decide_disabled(&MockEnv::new()), None);
    }

    #[test]
    fn do_not_track_disables() {
        let env = MockEnv::new().set("DO_NOT_TRACK", "1");
        assert_eq!(decide_disabled(&env), Some(DisableReason::DoNotTrack));
    }

    #[test]
    fn do_not_track_zero_does_not_disable() {
        let env = MockEnv::new().set("DO_NOT_TRACK", "0");
        assert_eq!(decide_disabled(&env), None);
    }

    #[test]
    fn do_not_track_empty_does_not_disable() {
        let env = MockEnv::new().set("DO_NOT_TRACK", "");
        assert_eq!(decide_disabled(&env), None);
    }

    #[test]
    fn aura_telemetry_disabled_disables() {
        let env = MockEnv::new().set("AURA_TELEMETRY_DISABLED", "1");
        assert_eq!(decide_disabled(&env), Some(DisableReason::AuraDisabled));
    }

    #[test]
    fn github_actions_disables() {
        let env = MockEnv::new().set("GITHUB_ACTIONS", "true");
        assert_eq!(
            decide_disabled(&env),
            Some(DisableReason::Ci("GITHUB_ACTIONS"))
        );
    }

    #[test]
    fn generic_ci_disables() {
        let env = MockEnv::new().set("CI", "true");
        assert_eq!(decide_disabled(&env), Some(DisableReason::Ci("CI")));
    }

    #[test]
    fn each_ci_provider_disables() {
        for name in CI_ENV_VARS {
            let env = MockEnv::new().set(name, "true");
            assert_eq!(
                decide_disabled(&env),
                Some(DisableReason::Ci(name)),
                "expected CI provider {name} to disable telemetry"
            );
        }
    }

    #[test]
    fn cargo_target_tmpdir_disables() {
        let env = MockEnv::new().set("CARGO_TARGET_TMPDIR", "/tmp/whatever");
        assert_eq!(decide_disabled(&env), Some(DisableReason::CargoTest));
    }

    #[test]
    fn rust_test_threads_disables() {
        let env = MockEnv::new().set("RUST_TEST_THREADS", "1");
        assert_eq!(decide_disabled(&env), Some(DisableReason::CargoTest));
    }

    #[test]
    fn aura_telemetry_enabled_false_disables() {
        let env = MockEnv::new().set("AURA_TELEMETRY_ENABLED", "false");
        assert_eq!(decide_disabled(&env), Some(DisableReason::ConfigDisabled));
    }

    #[test]
    fn aura_telemetry_enabled_true_does_not_disable() {
        let env = MockEnv::new().set("AURA_TELEMETRY_ENABLED", "true");
        assert_eq!(decide_disabled(&env), None);
    }

    // Precedence tests — critical: each upstream check must short-circuit
    // before downstream checks fire, so the recorded disable_reason
    // reflects user intent ("user explicitly opted out") vs. environment
    // ("we happened to be in CI").

    #[test]
    fn do_not_track_beats_ci() {
        let env = MockEnv::new()
            .set("DO_NOT_TRACK", "1")
            .set("CI", "true")
            .set("GITHUB_ACTIONS", "true");
        assert_eq!(decide_disabled(&env), Some(DisableReason::DoNotTrack));
    }

    #[test]
    fn do_not_track_beats_aura_disabled() {
        let env = MockEnv::new()
            .set("DO_NOT_TRACK", "1")
            .set("AURA_TELEMETRY_DISABLED", "1");
        assert_eq!(decide_disabled(&env), Some(DisableReason::DoNotTrack));
    }

    #[test]
    fn aura_disabled_beats_ci() {
        let env = MockEnv::new()
            .set("AURA_TELEMETRY_DISABLED", "1")
            .set("CI", "true");
        assert_eq!(decide_disabled(&env), Some(DisableReason::AuraDisabled));
    }

    #[test]
    fn ci_beats_cargo_test() {
        let env = MockEnv::new()
            .set("CI", "true")
            .set("CARGO_TARGET_TMPDIR", "/tmp");
        assert_eq!(decide_disabled(&env), Some(DisableReason::Ci("CI")));
    }

    #[test]
    fn ci_provider_order_is_deterministic() {
        // When multiple CI vars are set, the first one in CI_ENV_VARS wins.
        let env = MockEnv::new()
            .set("CI", "true")
            .set("GITHUB_ACTIONS", "true")
            .set("BUILDKITE", "true");
        assert_eq!(decide_disabled(&env), Some(DisableReason::Ci("CI")));
    }
}
