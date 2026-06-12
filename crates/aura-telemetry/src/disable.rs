//! Kill-switch decision tree.
//!
//! Evaluated exactly once at telemetry init. First match wins; subsequent
//! checks are skipped. The order is intentional and load-bearing — see
//! `docs/telemetry.md` for the user-facing precedence table.

/// The three telemetry states (spec revision 2026-06-03).
///
/// Telemetry is **notice-gated**: it does not send until a preference is
/// recorded (interactively via the first-run notice, or explicitly via
/// config/env). See `docs/telemetry.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryState {
    /// No preference recorded. Telemetry is **held** — events are written
    /// to the local inspection log only, never queued to the sink, and
    /// never backfilled after a later transition to `Enabled`.
    Unknown,
    /// A first-run notice was presented and not opted out of, or an
    /// operator explicitly enabled via config/env. Telemetry may be sent.
    Enabled,
    /// A kill switch or explicit opt-out is in effect. Telemetry is held.
    Disabled(DisableReason),
}

/// Reason a telemetry run is held in the `Disabled` state.
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

    /// The user's home directory, used to root `~/.aura/install-id` and
    /// the inspection log. The default reads the `HOME` env var (so test
    /// providers work without overriding), but [`SystemEnv`] overrides
    /// this to use `dirs::home_dir()` — which resolves `USERPROFILE` on
    /// Windows, where `HOME` is typically unset. This keeps telemetry
    /// state co-located with the rest of the CLI's `~/.aura` (which also
    /// resolves via `dirs`) instead of scattering a per-cwd install-id.
    fn home_dir(&self) -> Option<std::path::PathBuf> {
        self.var("HOME")
            .filter(|h| !h.is_empty())
            .map(std::path::PathBuf::from)
    }
}

/// Read-from-the-real-environment provider. Default outside tests.
pub struct SystemEnv;

impl EnvProvider for SystemEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn home_dir(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir()
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

/// Common false-y values, case-insensitive, that should *not* trigger
/// a boolean env-var kill switch. Anything not in this set (and not
/// empty / unset) is treated as "the user enabled the flag", so
/// `DO_NOT_TRACK=enabled` keeps working even though we don't enumerate
/// every truthy spelling.
///
/// Without this, `DO_NOT_TRACK=false` or `CI=false` (a common
/// shell-rc pattern for "I'm not in CI today") would silently
/// suppress all telemetry — the opposite of the user's intent.
/// The recognized false-y values for env-var booleans, case-insensitive.
/// `pub(crate)` so the bootstrap config parsing applies exactly the same
/// rule as the kill-switch parsing here — the two must never diverge.
pub(crate) fn is_false_value(s: &str) -> bool {
    let lower = s.trim().to_ascii_lowercase();
    matches!(lower.as_str(), "" | "0" | "false" | "no" | "off")
}

/// Treat the env var as "the user enabled this kill switch". Unset and
/// recognized false values do not count.
fn flag_set(v: &Option<String>) -> bool {
    match v {
        None => false,
        Some(s) => !is_false_value(s),
    }
}

/// Tier-1 "hard" disables that can never be overridden by an explicit
/// enable: the industry/Aura opt-out env vars, CI, and cargo-test.
///
/// Kept separate from the lower-precedence config enable/disable so a
/// misconfigured `AURA_TELEMETRY_ENABLED=true` in CI still cannot send —
/// the spec lists "counts materially influenced by CI/dev/test" as a
/// failure mode. Precedence within the tier: `DO_NOT_TRACK` →
/// `AURA_TELEMETRY_DISABLED` → CI envs → cargo-test markers.
///
/// Returns `Some(reason)` if a hard disable is active, else `None`.
pub fn decide_hard_disable(env: &dyn EnvProvider) -> Option<DisableReason> {
    if flag_set(&env.var("DO_NOT_TRACK")) {
        return Some(DisableReason::DoNotTrack);
    }
    if flag_set(&env.var("AURA_TELEMETRY_DISABLED")) {
        return Some(DisableReason::AuraDisabled);
    }
    for name in CI_ENV_VARS {
        if flag_set(&env.var(name)) {
            return Some(DisableReason::Ci(name));
        }
    }
    if env.var("CARGO_TARGET_TMPDIR").is_some() || env.var("RUST_TEST_THREADS").is_some() {
        return Some(DisableReason::CargoTest);
    }
    None
}

/// Resolve the [`TelemetryState`] from the environment and the recorded
/// `[telemetry] enabled` preference (`None` = no preference recorded).
///
/// Precedence (highest first):
/// 1. Hard disables ([`decide_hard_disable`]) → `Disabled`, even over an
///    explicit enable.
/// 2. `AURA_TELEMETRY_ENABLED` env (non-empty): false-value → `Disabled`,
///    truthy → `Enabled`. Env wins over the file preference.
/// 3. File preference: `Some(false)` → `Disabled`, `Some(true)` →
///    `Enabled`.
/// 4. Otherwise → `Unknown` (held; awaiting a notice or explicit enable).
pub fn decide_state(env: &dyn EnvProvider, file_enabled: Option<bool>) -> TelemetryState {
    if let Some(reason) = decide_hard_disable(env) {
        return TelemetryState::Disabled(reason);
    }
    if let Some(v) = env
        .var("AURA_TELEMETRY_ENABLED")
        .filter(|s| !s.trim().is_empty())
    {
        return if is_false_value(&v) {
            TelemetryState::Disabled(DisableReason::ConfigDisabled)
        } else {
            TelemetryState::Enabled
        };
    }
    match file_enabled {
        Some(false) => TelemetryState::Disabled(DisableReason::ConfigDisabled),
        Some(true) => TelemetryState::Enabled,
        None => TelemetryState::Unknown,
    }
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

    // ---- decide_hard_disable (tier-1 kill switches) ----

    #[test]
    fn hard_empty_env_is_none() {
        assert_eq!(decide_hard_disable(&MockEnv::new()), None);
    }

    #[test]
    fn hard_do_not_track_disables() {
        let env = MockEnv::new().set("DO_NOT_TRACK", "1");
        assert_eq!(decide_hard_disable(&env), Some(DisableReason::DoNotTrack));
    }

    /// Regression: `DO_NOT_TRACK=false` / `CI=false` must NOT trigger a
    /// kill switch (common shell-rc pattern for "not in CI today").
    #[test]
    fn hard_false_values_do_not_disable() {
        for v in ["false", "FALSE", "no", "NO", "off", "0", ""] {
            for key in ["DO_NOT_TRACK", "AURA_TELEMETRY_DISABLED", "CI"] {
                let env = MockEnv::new().set(key, v);
                assert_eq!(
                    decide_hard_disable(&env),
                    None,
                    "{key}={v:?} must not be a hard disable"
                );
            }
        }
    }

    #[test]
    fn hard_assorted_truthy_values_disable() {
        for v in ["1", "true", "yes", "on", "enabled", "please", "TRUE"] {
            let env = MockEnv::new().set("DO_NOT_TRACK", v);
            assert_eq!(
                decide_hard_disable(&env),
                Some(DisableReason::DoNotTrack),
                "DO_NOT_TRACK={v:?} should hard-disable"
            );
        }
    }

    #[test]
    fn hard_aura_disabled() {
        let env = MockEnv::new().set("AURA_TELEMETRY_DISABLED", "1");
        assert_eq!(decide_hard_disable(&env), Some(DisableReason::AuraDisabled));
    }

    #[test]
    fn hard_each_ci_provider() {
        for name in CI_ENV_VARS {
            let env = MockEnv::new().set(name, "true");
            assert_eq!(
                decide_hard_disable(&env),
                Some(DisableReason::Ci(name)),
                "expected CI provider {name} to hard-disable"
            );
        }
    }

    #[test]
    fn hard_cargo_markers() {
        assert_eq!(
            decide_hard_disable(&MockEnv::new().set("CARGO_TARGET_TMPDIR", "/tmp/x")),
            Some(DisableReason::CargoTest)
        );
        assert_eq!(
            decide_hard_disable(&MockEnv::new().set("RUST_TEST_THREADS", "1")),
            Some(DisableReason::CargoTest)
        );
    }

    /// `AURA_TELEMETRY_ENABLED=false` is a tier-2 config disable, NOT a
    /// tier-1 hard disable — it must not appear in `decide_hard_disable`.
    #[test]
    fn hard_does_not_include_aura_telemetry_enabled() {
        let env = MockEnv::new().set("AURA_TELEMETRY_ENABLED", "false");
        assert_eq!(decide_hard_disable(&env), None);
    }

    #[test]
    fn hard_precedence() {
        // DNT beats AuraDisabled beats CI beats cargo; first CI var wins.
        assert_eq!(
            decide_hard_disable(
                &MockEnv::new()
                    .set("DO_NOT_TRACK", "1")
                    .set("AURA_TELEMETRY_DISABLED", "1")
                    .set("CI", "true")
            ),
            Some(DisableReason::DoNotTrack)
        );
        assert_eq!(
            decide_hard_disable(
                &MockEnv::new()
                    .set("AURA_TELEMETRY_DISABLED", "1")
                    .set("CI", "true")
            ),
            Some(DisableReason::AuraDisabled)
        );
        assert_eq!(
            decide_hard_disable(
                &MockEnv::new()
                    .set("CI", "true")
                    .set("CARGO_TARGET_TMPDIR", "/tmp")
            ),
            Some(DisableReason::Ci("CI"))
        );
        assert_eq!(
            decide_hard_disable(
                &MockEnv::new()
                    .set("CI", "true")
                    .set("GITHUB_ACTIONS", "true")
                    .set("BUILDKITE", "true")
            ),
            Some(DisableReason::Ci("CI"))
        );
    }

    // ---- decide_state (tri-state resolution) ----

    #[test]
    fn state_unknown_when_nothing_recorded() {
        assert_eq!(decide_state(&MockEnv::new(), None), TelemetryState::Unknown);
    }

    #[test]
    fn state_file_true_enables() {
        assert_eq!(
            decide_state(&MockEnv::new(), Some(true)),
            TelemetryState::Enabled
        );
    }

    #[test]
    fn state_file_false_disables() {
        assert_eq!(
            decide_state(&MockEnv::new(), Some(false)),
            TelemetryState::Disabled(DisableReason::ConfigDisabled)
        );
    }

    #[test]
    fn state_env_enabled_truthy() {
        for v in ["true", "yes", "on", "1", "enabled"] {
            let env = MockEnv::new().set("AURA_TELEMETRY_ENABLED", v);
            assert_eq!(
                decide_state(&env, None),
                TelemetryState::Enabled,
                "AURA_TELEMETRY_ENABLED={v:?} should enable"
            );
        }
    }

    #[test]
    fn state_env_enabled_false_disables() {
        for v in ["false", "FALSE", "no", "off", "0"] {
            let env = MockEnv::new().set("AURA_TELEMETRY_ENABLED", v);
            assert_eq!(
                decide_state(&env, None),
                TelemetryState::Disabled(DisableReason::ConfigDisabled),
                "AURA_TELEMETRY_ENABLED={v:?} should disable"
            );
        }
    }

    #[test]
    fn state_env_empty_falls_through_to_file() {
        // `AURA_TELEMETRY_ENABLED=` (empty) is treated as unset, so the
        // file preference (here: none) decides.
        let env = MockEnv::new().set("AURA_TELEMETRY_ENABLED", "");
        assert_eq!(decide_state(&env, None), TelemetryState::Unknown);
        assert_eq!(decide_state(&env, Some(true)), TelemetryState::Enabled);
    }

    #[test]
    fn state_env_wins_over_file() {
        let enabled = MockEnv::new().set("AURA_TELEMETRY_ENABLED", "true");
        assert_eq!(decide_state(&enabled, Some(false)), TelemetryState::Enabled);
        let disabled = MockEnv::new().set("AURA_TELEMETRY_ENABLED", "false");
        assert_eq!(
            decide_state(&disabled, Some(true)),
            TelemetryState::Disabled(DisableReason::ConfigDisabled)
        );
    }

    /// The load-bearing guarantee: a hard disable beats an explicit
    /// enable from either env or file. A misconfigured
    /// `AURA_TELEMETRY_ENABLED=true` in CI must still not send.
    #[test]
    fn state_hard_disable_beats_explicit_enable() {
        let dnt_plus_file = MockEnv::new().set("DO_NOT_TRACK", "1");
        assert_eq!(
            decide_state(&dnt_plus_file, Some(true)),
            TelemetryState::Disabled(DisableReason::DoNotTrack)
        );
        let dnt_plus_env = MockEnv::new()
            .set("DO_NOT_TRACK", "1")
            .set("AURA_TELEMETRY_ENABLED", "true");
        assert_eq!(
            decide_state(&dnt_plus_env, None),
            TelemetryState::Disabled(DisableReason::DoNotTrack)
        );
        let ci_plus_env = MockEnv::new()
            .set("CI", "true")
            .set("AURA_TELEMETRY_ENABLED", "true");
        assert_eq!(
            decide_state(&ci_plus_env, Some(true)),
            TelemetryState::Disabled(DisableReason::Ci("CI"))
        );
    }
}
