//! Default-resolution helpers for `aura-cli`.
//!
//! These exist so the CLI integration site has one place that resolves
//! env vars, default paths, and the kill-switch decision. The
//! [`build_config_from_env`] function returns a fully populated
//! [`TelemetryConfig`]; callers pass it straight into [`crate::init`].
//!
//! Anything env-driven goes through this module so a future audit of
//! "which env vars affect telemetry" only needs to grep here. Telemetry
//! is CLI-only: paths root under the user's `~/.aura`, and `Source` is
//! always [`Source::Cli`].

use std::path::PathBuf;

use uuid::Uuid;

use crate::disable::{decide_state, is_false_value, EnvProvider, SystemEnv, TelemetryState};
use crate::install_id;
use crate::properties::{DeploymentMethod, OsFamily, Source};
use crate::{FileTelemetryConfig, TelemetryConfig};

/// PostHog Cloud US ingest endpoint. Self-hosters override with
/// `AURA_TELEMETRY_ENDPOINT` or the `[telemetry] endpoint = "…"` config
/// field (the latter wired by callers, not by this module).
pub const DEFAULT_ENDPOINT: &str = "https://us.i.posthog.com";

/// Aura's PostHog project key, embedded as the default.
///
/// PostHog public project keys are write-only by design: they can emit
/// events but cannot read any back, so embedding one in OSS source is
/// safe. A build-time `AURA_TELEMETRY_BUILD_API_KEY` overrides it (e.g.
/// to point a fork or a staging build at a different project), and the
/// runtime `AURA_TELEMETRY_API_KEY` / `[telemetry] api_key` override that
/// in turn. If a build ever clears the key to empty, PostHog 401s the
/// requests; the failures are logged at `tracing::debug!` and never
/// break Aura.
pub const DEFAULT_API_KEY: &str = match option_env!("AURA_TELEMETRY_BUILD_API_KEY") {
    Some(k) => k,
    None => "phc_xAfjuMnogxwXpYmLbRRySHeZq99TBHEZMz3VmYXq5V3k",
};

/// Resolve the install-id persistence path: `$HOME/.aura/install-id`
/// (the path users see documented in `docs/telemetry.md`), falling back
/// to a per-cwd `.aura/install-id` only when no home directory resolves.
///
/// `HOME` is read through the injected [`EnvProvider`] so unit tests can
/// supply a `MockEnv` and keep their install-id inside a tempdir instead
/// of writing to the developer's real `~/.aura/install-id`.
pub fn resolve_install_id_path(env: &dyn EnvProvider) -> PathBuf {
    home_install_id(env).unwrap_or_else(|| PathBuf::from(".aura").join("install-id"))
}

/// `{home}/.aura/install-id` when a home directory can be resolved.
/// Home resolution goes through [`EnvProvider::home_dir`], which is
/// `dirs::home_dir()` in production (Windows-correct) and HOME-based in
/// tests.
fn home_install_id(env: &dyn EnvProvider) -> Option<PathBuf> {
    env.home_dir()
        .map(|home| home.join(".aura").join("install-id"))
}

/// Whether the install-id resolved for these inputs lands in a
/// **durable** location (the user's home), as opposed to the bare-cwd
/// fallback used when no home directory resolves. A non-durable location
/// means a fresh install UUID per working directory, which churns the
/// install count — so [`build_config_with_env`] warns when telemetry is
/// active and the location is not durable.
fn install_id_is_durable(env: &dyn EnvProvider) -> bool {
    env.home_dir().is_some()
}

/// Resolve the local inspection-log path. Returns `None` when the user
/// has opted out of the inspection log via `AURA_TELEMETRY_LOG_EVENTS`
/// set to a recognized false value (`0`, `false`, `no`, `off`,
/// case-insensitive). This matches the parsing rules
/// [`crate::disable`] applies to the wire-side kill switches so a user
/// who sets `=false` (or `=off`) sees the same outcome there as here.
pub fn resolve_inspection_log_path(env: &dyn EnvProvider) -> Option<PathBuf> {
    if let Some(v) = env.var("AURA_TELEMETRY_LOG_EVENTS") {
        if is_false_value(&v) {
            return None;
        }
    }
    Some(default_inspection_log_path(env))
}

/// Resolve a setting by precedence: a non-empty env value wins over a
/// non-empty file value, falling back to the built-in default.
/// Single-sources the env-over-file-over-default rule (with empty-as-unset
/// filtering) shared by `endpoint` and `api_key`.
fn resolve_setting(env_value: Option<String>, file_value: Option<String>, default: &str) -> String {
    env_value
        .filter(|s| !s.is_empty())
        .or_else(|| file_value.filter(|s| !s.is_empty()))
        .unwrap_or_else(|| default.to_string())
}

fn default_inspection_log_path(env: &dyn EnvProvider) -> PathBuf {
    if let Some(home) = env.home_dir() {
        return home.join(".aura").join("telemetry").join("events.jsonl");
    }
    PathBuf::from(".aura")
        .join("telemetry")
        .join("events.jsonl")
}

/// Resolve a [`TelemetryConfig`] from the environment + the disable
/// decision tree. Equivalent to `build_config_from_env_and_file(None)`.
pub fn build_config_from_env() -> TelemetryConfig {
    build_config_from_env_and_file(None)
}

/// Resolve a [`TelemetryConfig`] from env vars and an optional
/// `[telemetry]` block parsed from `cli.toml`. Precedence per
/// `docs/telemetry.md`: env wins over file wins over built-in defaults.
/// The `enabled = false` file-config kill switch fires only when no
/// higher-precedence kill switch (env or auto-disable) already took
/// effect.
pub fn build_config_from_env_and_file(file: Option<&FileTelemetryConfig>) -> TelemetryConfig {
    let env = SystemEnv;
    build_config_with_env(file, &env)
}

/// Same as [`build_config_from_env_and_file`] but takes an explicit env
/// provider for unit testability.
pub fn build_config_with_env(
    file: Option<&FileTelemetryConfig>,
    env: &dyn EnvProvider,
) -> TelemetryConfig {
    let endpoint = resolve_setting(
        env.var("AURA_TELEMETRY_ENDPOINT"),
        file.and_then(|f| f.endpoint.clone()),
        DEFAULT_ENDPOINT,
    );
    let api_key = resolve_setting(
        env.var("AURA_TELEMETRY_API_KEY"),
        file.and_then(|f| f.api_key.clone()),
        DEFAULT_API_KEY,
    );
    let deployment_method = DeploymentMethod::parse(env.var("AURA_DEPLOYMENT_METHOD").as_deref());

    // Resolve the tri-state: hard kill switches + CI/test → Disabled
    // (even over an explicit enable); `AURA_TELEMETRY_ENABLED` / config
    // `enabled` → Enabled or Disabled; otherwise Unknown (held until a
    // notice or explicit enable). The recorded preference is the file's
    // `[telemetry] enabled` field.
    let state = decide_state(env, file.and_then(|f| f.enabled));

    // Resolve the install-id path either way (so `/telemetry status`
    // can show users where it WOULD live). Touch the filesystem for
    // Enabled AND Unknown (Unknown needs a stable id ready for a later
    // `enable()` transition; the write never implies a send), but never
    // for Disabled — a disabled run uses an ephemeral per-run UUID so
    // `cargo test` subprocesses can't pollute `~/.aura/install-id`.
    let install_id_path = resolve_install_id_path(env);
    let durable_location = install_id_is_durable(env);
    let mut persisted = false;
    let install_id = if matches!(state, TelemetryState::Disabled(_)) {
        Uuid::new_v4()
    } else {
        match install_id::read_or_create(&install_id_path) {
            Ok(uuid) => {
                persisted = durable_location;
                uuid
            }
            Err(e) => {
                tracing::debug!(error = %e, path = %install_id_path.display(),
                    "could not persist install-id; telemetry will use a one-off UUID");
                Uuid::new_v4()
            }
        }
    };

    // When telemetry is **Enabled** but the install-id won't survive
    // (no resolvable home → per-cwd UUID), the install count churns.
    // Warn loudly. Unknown/Disabled never send, so they don't warn.
    if matches!(state, TelemetryState::Enabled) && !persisted {
        tracing::warn!(
            path = %install_id_path.display(),
            "telemetry install-id is not on persistent storage (no home dir \
             resolved); the install count will be unstable across runs. See \
             docs/telemetry.md."
        );
    }

    let inspection_log_path = resolve_inspection_log_path(env);

    let mut cfg = TelemetryConfig::default_for(
        Source::Cli,
        install_id,
        endpoint,
        api_key,
        inspection_log_path,
    );
    cfg.install_id_path = Some(install_id_path);
    cfg.deployment_method = deployment_method;
    cfg.os_family = OsFamily::current();
    cfg.state = state;
    cfg
}

/// Best-effort one-line startup summary of the telemetry state.
pub fn startup_log_line(state: &TelemetryState) -> String {
    use crate::inspection_log::disable_reason_label;
    match state {
        TelemetryState::Unknown => {
            "telemetry: unknown (held — awaiting notice or explicit enable)".to_string()
        }
        TelemetryState::Enabled => "telemetry: active".to_string(),
        TelemetryState::Disabled(r) => {
            format!("telemetry: disabled ({})", disable_reason_label(r))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disable::DisableReason;
    use std::collections::HashMap;

    #[derive(Default)]
    struct MockEnv(HashMap<String, String>);
    impl MockEnv {
        fn set(mut self, k: &str, v: &str) -> Self {
            self.0.insert(k.into(), v.into());
            self
        }
    }
    impl EnvProvider for MockEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        /// Models `dirs::home_dir()`: prefer `USERPROFILE` (Windows),
        /// fall back to `HOME` (Unix). Lets a test model a Windows
        /// account where `HOME` is unset.
        fn home_dir(&self) -> Option<std::path::PathBuf> {
            self.var("USERPROFILE")
                .or_else(|| self.var("HOME"))
                .filter(|h| !h.is_empty())
                .map(std::path::PathBuf::from)
        }
    }

    /// Regression: on Windows `HOME` is typically unset (the home dir
    /// comes from `USERPROFILE`/`dirs::home_dir()`). The resolvers used
    /// to read `HOME` directly, so a Windows CLI fell through to a
    /// per-cwd `.aura/install-id` — a fresh install UUID per directory.
    /// Routing home resolution through `EnvProvider::home_dir` fixes it.
    #[test]
    fn install_id_roots_under_home_dir_when_home_env_unset() {
        let env = MockEnv::default().set("USERPROFILE", "/winhome/user");
        let resolved = resolve_install_id_path(&env);
        assert_eq!(
            resolved,
            std::path::PathBuf::from("/winhome/user")
                .join(".aura")
                .join("install-id"),
            "install-id must root under the resolved home dir, not cwd"
        );
        // And such a run is durable (a real home), so no churn warning.
        assert!(install_id_is_durable(&env));
    }

    #[test]
    fn resolve_inspection_log_zero_disables() {
        let env = MockEnv::default().set("AURA_TELEMETRY_LOG_EVENTS", "0");
        assert!(resolve_inspection_log_path(&env).is_none());
    }

    #[test]
    fn resolve_inspection_log_non_zero_does_not_disable() {
        let env = MockEnv::default().set("AURA_TELEMETRY_LOG_EVENTS", "1");
        assert!(resolve_inspection_log_path(&env).is_some());
    }

    /// Same fix as `disable.rs::do_not_track_false_values_do_not_disable`:
    /// false-y string values disable the inspection log, mirroring the
    /// wire-side kill-switch parsing. Without this, `=false` would have
    /// kept the log on (because the old check was exact-match on `=0`).
    #[test]
    fn resolve_inspection_log_recognizes_false_values() {
        for v in ["false", "FALSE", "no", "off", "0", ""] {
            let env = MockEnv::default().set("AURA_TELEMETRY_LOG_EVENTS", v);
            assert!(
                resolve_inspection_log_path(&env).is_none(),
                "AURA_TELEMETRY_LOG_EVENTS={v:?} should disable the inspection log"
            );
        }
    }

    #[test]
    fn resolve_inspection_log_truthy_values_leave_log_on() {
        for v in ["true", "1", "yes", "on", "enabled"] {
            let env = MockEnv::default().set("AURA_TELEMETRY_LOG_EVENTS", v);
            assert!(
                resolve_inspection_log_path(&env).is_some(),
                "AURA_TELEMETRY_LOG_EVENTS={v:?} should leave the inspection log on"
            );
        }
    }

    #[test]
    fn inspection_log_roots_under_home() {
        let home = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", &home.path().to_string_lossy());
        let path = resolve_inspection_log_path(&env).unwrap();
        assert!(
            path.starts_with(home.path()),
            "inspection log should root under $HOME, got {}",
            path.display()
        );
        assert!(path.ends_with("telemetry/events.jsonl"));
    }

    #[test]
    fn build_config_honors_env_endpoint() {
        let home = tempfile::tempdir().unwrap();
        let env = MockEnv::default()
            .set("HOME", &home.path().to_string_lossy())
            .set("AURA_TELEMETRY_ENDPOINT", "https://posthog.example/")
            .set("AURA_TELEMETRY_API_KEY", "phc_unit_test");
        let cfg = build_config_with_env(None, &env);
        assert_eq!(cfg.endpoint, "https://posthog.example/");
        assert_eq!(cfg.api_key, "phc_unit_test");
        assert_eq!(cfg.deployment_method, DeploymentMethod::Local);
        // Audit field is populated even when no env override fired.
        assert!(cfg.install_id_path.is_some());
    }

    #[test]
    fn build_config_picks_up_deployment_method() {
        let home = tempfile::tempdir().unwrap();
        let env = MockEnv::default()
            .set("HOME", &home.path().to_string_lossy())
            .set("AURA_DEPLOYMENT_METHOD", "standalone-cli");
        let cfg = build_config_with_env(None, &env);
        assert_eq!(cfg.deployment_method, DeploymentMethod::StandaloneCli);
    }

    #[test]
    fn build_config_records_disabled_state_from_env() {
        let env = MockEnv::default().set("DO_NOT_TRACK", "1");
        let cfg = build_config_with_env(None, &env);
        assert!(matches!(
            cfg.state,
            TelemetryState::Disabled(DisableReason::DoNotTrack)
        ));
    }

    #[test]
    fn no_preference_resolves_unknown() {
        // The behavioural inversion: with nothing recorded, the state is
        // Unknown (held), not Enabled. install-id is still resolved for
        // a later transition.
        let home = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", &home.path().to_string_lossy());
        let cfg = build_config_with_env(None, &env);
        assert!(matches!(cfg.state, TelemetryState::Unknown));
        assert!(cfg.install_id_path.is_some());
    }

    #[test]
    fn file_enabled_false_records_config_disabled_when_env_silent() {
        let env = MockEnv::default();
        let file = FileTelemetryConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let cfg = build_config_with_env(Some(&file), &env);
        assert!(matches!(
            cfg.state,
            TelemetryState::Disabled(DisableReason::ConfigDisabled)
        ));
    }

    #[test]
    fn env_disable_outranks_file_enable() {
        // env DoNotTrack must win even over an explicit file enable —
        // the hard-disable-beats-explicit-enable guarantee.
        let env = MockEnv::default().set("DO_NOT_TRACK", "1");
        let file = FileTelemetryConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let cfg = build_config_with_env(Some(&file), &env);
        assert!(matches!(
            cfg.state,
            TelemetryState::Disabled(DisableReason::DoNotTrack)
        ));
    }

    #[test]
    fn file_endpoint_used_when_env_unset() {
        let home = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", &home.path().to_string_lossy());
        let file = FileTelemetryConfig {
            endpoint: Some("https://self-hosted.example/posthog".into()),
            api_key: Some("phc_self".into()),
            ..Default::default()
        };
        let cfg = build_config_with_env(Some(&file), &env);
        assert_eq!(cfg.endpoint, "https://self-hosted.example/posthog");
        assert_eq!(cfg.api_key, "phc_self");
    }

    #[test]
    fn env_endpoint_outranks_file_endpoint() {
        let home = tempfile::tempdir().unwrap();
        let env = MockEnv::default()
            .set("HOME", &home.path().to_string_lossy())
            .set("AURA_TELEMETRY_ENDPOINT", "https://env-wins.example/");
        let file = FileTelemetryConfig {
            endpoint: Some("https://file-loses.example/".into()),
            ..Default::default()
        };
        let cfg = build_config_with_env(Some(&file), &env);
        assert_eq!(cfg.endpoint, "https://env-wins.example/");
    }

    #[test]
    fn file_enabled_true_is_explicit_enable() {
        // Under the consent model `enabled = true` is the user's
        // explicit opt-in → Enabled (not a no-op as in the old opt-out
        // model where the default was already on).
        let home = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", &home.path().to_string_lossy());
        let file = FileTelemetryConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let cfg = build_config_with_env(Some(&file), &env);
        assert!(matches!(cfg.state, TelemetryState::Enabled));
    }

    #[test]
    fn startup_log_line_covers_three_states() {
        assert_eq!(
            startup_log_line(&TelemetryState::Enabled),
            "telemetry: active"
        );
        assert_eq!(
            startup_log_line(&TelemetryState::Disabled(DisableReason::DoNotTrack)),
            "telemetry: disabled (DoNotTrack)"
        );
        assert!(startup_log_line(&TelemetryState::Unknown).contains("unknown"));
    }

    /// Regression: `resolve_install_id_path` used to read
    /// `std::env::var_os("HOME")` directly, bypassing the injected
    /// `EnvProvider`. As a result, unit tests calling
    /// `build_config_with_env` could touch the developer's real
    /// `~/.aura/install-id`. The fix routes HOME through the env
    /// provider; this test pins it by passing a per-test HOME and
    /// confirming the resolved path roots there.
    #[test]
    fn install_id_path_honors_injected_home() {
        let fake_home = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", &fake_home.path().to_string_lossy());
        let resolved = resolve_install_id_path(&env);
        assert!(
            resolved.starts_with(fake_home.path()),
            "install-id path must root under the injected HOME, got {}",
            resolved.display()
        );
        assert!(resolved.ends_with(".aura/install-id"));
    }

    #[test]
    fn install_id_path_falls_back_to_cwd_when_home_unset() {
        let env = MockEnv::default(); // no HOME
        let resolved = resolve_install_id_path(&env);
        assert_eq!(
            resolved,
            std::path::PathBuf::from(".aura").join("install-id")
        );
    }

    #[test]
    fn install_id_durable_with_home() {
        let env = MockEnv::default().set("HOME", "/home/dev");
        assert!(install_id_is_durable(&env));
    }

    #[test]
    fn install_id_not_durable_without_home() {
        // No HOME → bare-cwd fallback → not durable.
        let env = MockEnv::default();
        assert!(!install_id_is_durable(&env));
    }

    #[test]
    fn install_id_not_durable_with_empty_home() {
        let env = MockEnv::default().set("HOME", "");
        assert!(!install_id_is_durable(&env));
    }

    /// The companion guarantee on the wire side: when telemetry is
    /// disabled by an env-level kill switch, `build_config_with_env`
    /// must not write the install-id file. Tests that previously
    /// reached install-id creation via the disabled path (because the
    /// reorder hadn't happened yet) could end up creating the file
    /// in the real HOME if HOME wasn't routed through the mock env.
    #[test]
    fn disabled_run_does_not_touch_install_id_file() {
        let fake_home = tempfile::tempdir().unwrap();
        let env = MockEnv::default()
            .set("HOME", &fake_home.path().to_string_lossy())
            .set("DO_NOT_TRACK", "1");
        let cfg = build_config_with_env(None, &env);
        assert!(matches!(
            cfg.state,
            TelemetryState::Disabled(DisableReason::DoNotTrack)
        ));
        // The file is NOT created; the path is still surfaced for
        // /telemetry status.
        let expected_path = fake_home.path().join(".aura").join("install-id");
        assert_eq!(
            cfg.install_id_path.as_deref(),
            Some(expected_path.as_path())
        );
        assert!(
            !expected_path.exists(),
            "disabled run must not create the install-id file"
        );
    }
}
