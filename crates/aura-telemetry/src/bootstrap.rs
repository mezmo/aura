//! Default-resolution helpers for `aura-web-server` and `aura-cli`.
//!
//! These exist so the two integration sites do not drift in how they
//! resolve env vars, default paths, or the kill-switch decision. The
//! [`build_config_from_env_and_file`] function returns a fully populated
//! [`TelemetryConfig`] given a `Source`, an optional `memory_dir`, and an
//! optional `[telemetry]` file block; callers pass it straight into
//! [`crate::init`].
//!
//! Anything env-driven goes through this module so a future audit of
//! "which env vars affect telemetry" only needs to grep here.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::disable::{decide_state, is_false_value, EnvProvider, SystemEnv, TelemetryState};
use crate::install_id;
use crate::properties::{DeploymentMethod, Source};
use crate::{FileTelemetryConfig, TelemetryConfig};

/// PostHog Cloud US ingest endpoint. Self-hosters override with
/// `AURA_TELEMETRY_ENDPOINT` or the `[telemetry] endpoint = "…"` config
/// field (the latter wired by callers, not by this module).
pub const DEFAULT_ENDPOINT: &str = "https://us.i.posthog.com";

/// Mezmo's production-install-counter project key.
///
/// PostHog public project keys are write-only by design — they cannot
/// read events, only emit them — so embedding one in OSS source is
/// safe. If unset at build time (no `AURA_TELEMETRY_BUILD_API_KEY` in
/// the build environment) the default is empty, which causes the
/// PostHog server to 401 our requests; the failures are logged at
/// `tracing::debug!` and never break Aura. Set
/// `AURA_TELEMETRY_API_KEY` at runtime to point at your own project.
pub const DEFAULT_API_KEY: &str = match option_env!("AURA_TELEMETRY_BUILD_API_KEY") {
    Some(k) => k,
    None => "",
};

/// Resolve the install-id persistence path. **Source-aware**, mirroring
/// [`default_inspection_log_path`] so the install-id and the inspection
/// log land under the same root.
///
/// - [`Source::WebServer`]: prefer `{memory_dir}/install-id`. The server's
///   durable location is the mounted volume (e.g. `/app/state`); its
///   `$HOME` is typically the ephemeral container layer, so persisting
///   the install-id there would churn the install count across container
///   recreation. Falls back to `$HOME/.aura/install-id`, then cwd.
/// - [`Source::Cli`]: prefer `$HOME/.aura/install-id` (the path users see
///   documented in `docs/telemetry.md`); `{memory_dir}` is only a
///   fallback for system accounts without `$HOME`, then cwd.
///
/// `HOME` is read through the injected [`EnvProvider`] so unit tests
/// can supply a `MockEnv` and keep their install-id inside a tempdir
/// instead of writing to the developer's real `~/.aura/install-id`.
pub fn resolve_install_id_path(
    source: Source,
    memory_dir: Option<&Path>,
    env: &dyn EnvProvider,
) -> PathBuf {
    match source {
        Source::WebServer => {
            if let Some(dir) = memory_dir {
                return dir.join("install-id");
            }
            if let Some(p) = home_install_id(env) {
                return p;
            }
        }
        Source::Cli => {
            if let Some(p) = home_install_id(env) {
                return p;
            }
            if let Some(dir) = memory_dir {
                return dir.join("install-id");
            }
        }
    }
    PathBuf::from(".aura").join("install-id")
}

/// `$HOME/.aura/install-id` when `HOME` is set and non-empty.
fn home_install_id(env: &dyn EnvProvider) -> Option<PathBuf> {
    env.var("HOME")
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".aura").join("install-id"))
}

/// Whether the install-id resolved for these inputs lands in a
/// **durable** location — one that survives container recreation and
/// image pulls. **Source-aware**, matching [`resolve_install_id_path`].
///
/// - [`Source::WebServer`]: durable **only** when an explicit `memory_dir`
///   (a mounted volume) is configured. The container's `$HOME` is the
///   ephemeral writable layer, so a server relying on it churns the
///   install count across recreation — when telemetry is active and no
///   `memory_dir` is set, [`build_config_with_env`] emits a one-time
///   warning nudging the operator to mount one (the documented durable
///   location for servers in `docs/telemetry.md`). A bare-host server
///   with a real `$HOME` but no `memory_dir` gets the same nudge; that
///   mild false positive is acceptable since `memory_dir` is the
///   sanctioned durable root either way.
/// - [`Source::Cli`]: durable when `$HOME` is set (the user's home) or
///   `memory_dir` is supplied; only the bare-cwd `.aura` fallback is not.
///
/// `install_id_durability_matches_path` locks the CLI predicate against
/// the path resolver so the two stay in sync.
fn install_id_is_durable(source: Source, memory_dir: Option<&Path>, env: &dyn EnvProvider) -> bool {
    match source {
        Source::WebServer => memory_dir.is_some(),
        Source::Cli => {
            let home_set = env.var("HOME").map(|h| !h.is_empty()).unwrap_or(false);
            home_set || memory_dir.is_some()
        }
    }
}

/// Resolve the local inspection-log path. Returns `None` when the user
/// has opted out of the inspection log via `AURA_TELEMETRY_LOG_EVENTS`
/// set to a recognized false value (`0`, `false`, `no`, `off`,
/// case-insensitive). This matches the parsing rules
/// [`crate::disable`] applies to the wire-side kill switches so a user
/// who sets `=false` (or `=off`) sees the same outcome there as here.
pub fn resolve_inspection_log_path(
    source: Source,
    memory_dir: Option<&Path>,
    env: &dyn EnvProvider,
) -> Option<PathBuf> {
    if let Some(v) = env.var("AURA_TELEMETRY_LOG_EVENTS") {
        if is_false_value(&v) {
            return None;
        }
    }
    Some(default_inspection_log_path(source, memory_dir, env))
}

fn default_inspection_log_path(
    source: Source,
    memory_dir: Option<&Path>,
    env: &dyn EnvProvider,
) -> PathBuf {
    match source {
        Source::WebServer => {
            // `{memory_dir}/telemetry/events.jsonl` if available;
            // otherwise the same fallback as the CLI.
            if let Some(dir) = memory_dir {
                return dir.join("telemetry").join("events.jsonl");
            }
        }
        Source::Cli => {}
    }
    if let Some(home) = env.var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".aura")
                .join("telemetry")
                .join("events.jsonl");
        }
    }
    PathBuf::from(".aura")
        .join("telemetry")
        .join("events.jsonl")
}

/// Resolve a [`TelemetryConfig`] from env vars, an optional
/// `memory_dir`, and an optional `[telemetry]` block parsed from the
/// caller's config file. Precedence per `docs/telemetry.md`: env wins
/// over file wins over built-in defaults. The `enabled = false`
/// file-config kill switch fires only when no higher-precedence
/// kill switch (env or auto-disable) already took effect.
pub fn build_config_from_env_and_file(
    source: Source,
    memory_dir: Option<&Path>,
    file: Option<&FileTelemetryConfig>,
) -> TelemetryConfig {
    let env = SystemEnv;
    build_config_with_env(source, memory_dir, file, &env)
}

/// Same as [`build_config_from_env_and_file`] but takes an explicit env
/// provider for unit testability.
pub fn build_config_with_env(
    source: Source,
    memory_dir: Option<&Path>,
    file: Option<&FileTelemetryConfig>,
    env: &dyn EnvProvider,
) -> TelemetryConfig {
    // env var (non-empty) wins over the file value (non-empty) wins over
    // the built-in default, for each of endpoint and api_key.
    fn resolve(
        env: &dyn EnvProvider,
        var: &str,
        file_value: Option<String>,
        default: &str,
    ) -> String {
        env.var(var)
            .filter(|s| !s.is_empty())
            .or_else(|| file_value.filter(|s| !s.is_empty()))
            .unwrap_or_else(|| default.to_string())
    }
    let endpoint = resolve(
        env,
        "AURA_TELEMETRY_ENDPOINT",
        file.and_then(|f| f.endpoint.clone()),
        DEFAULT_ENDPOINT,
    );
    let api_key = resolve(
        env,
        "AURA_TELEMETRY_API_KEY",
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
    let install_id_path = resolve_install_id_path(source, memory_dir, env);
    let durable_location = install_id_is_durable(source, memory_dir, env);
    let mut persisted = false;
    let install_id = if matches!(state, TelemetryState::Disabled(_)) {
        Uuid::new_v4()
    } else {
        match install_id::read_or_create(&install_id_path) {
            Ok(install) => {
                persisted = durable_location;
                install.id()
            }
            Err(e) => {
                tracing::debug!(error = %e, path = %install_id_path.display(),
                    "could not persist install-id; telemetry will use a one-off UUID");
                Uuid::new_v4()
            }
        }
    };

    // When telemetry is **Enabled** but the install-id won't survive a
    // restart/recreation, the install count churns. Warn loudly — the
    // dumb-fix nudge is to set `memory_dir` on a persistent volume.
    // Unknown/Disabled never send, so they don't warn.
    if matches!(state, TelemetryState::Enabled) && !persisted {
        tracing::warn!(
            path = %install_id_path.display(),
            "telemetry install-id is not on persistent storage; the install \
             count will be unstable across restarts. Set `memory_dir` to a \
             mounted volume (or, in a stateless container, persist that path) \
             to stabilise it. See docs/telemetry.md."
        );
    }

    let inspection_log_path = resolve_inspection_log_path(source, memory_dir, env);

    let mut cfg =
        TelemetryConfig::default_for(source, install_id, endpoint, api_key, inspection_log_path);
    cfg.install_id_path = Some(install_id_path);
    cfg.deployment_method = deployment_method;
    cfg.state = state;
    cfg
}

/// Best-effort one-line startup summary of the telemetry state.
pub fn startup_log_line(state: &TelemetryState) -> String {
    match state {
        TelemetryState::Unknown => {
            "telemetry: unknown (held — awaiting notice or explicit enable)".to_string()
        }
        TelemetryState::Enabled => "telemetry: active".to_string(),
        TelemetryState::Disabled(r) => format!("telemetry: disabled ({r})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disable::DisableReason;
    use crate::test_support::MockEnv;

    #[test]
    fn resolve_inspection_log_zero_disables() {
        let env = MockEnv::default().set("AURA_TELEMETRY_LOG_EVENTS", "0");
        assert!(resolve_inspection_log_path(Source::WebServer, None, &env).is_none());
    }

    #[test]
    fn resolve_inspection_log_non_zero_does_not_disable() {
        let env = MockEnv::default().set("AURA_TELEMETRY_LOG_EVENTS", "1");
        assert!(resolve_inspection_log_path(Source::WebServer, None, &env).is_some());
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
                resolve_inspection_log_path(Source::WebServer, None, &env).is_none(),
                "AURA_TELEMETRY_LOG_EVENTS={v:?} should disable the inspection log"
            );
        }
    }

    #[test]
    fn resolve_inspection_log_truthy_values_leave_log_on() {
        for v in ["true", "1", "yes", "on", "enabled"] {
            let env = MockEnv::default().set("AURA_TELEMETRY_LOG_EVENTS", v);
            assert!(
                resolve_inspection_log_path(Source::WebServer, None, &env).is_some(),
                "AURA_TELEMETRY_LOG_EVENTS={v:?} should leave the inspection log on"
            );
        }
    }

    #[test]
    fn web_server_inspection_log_uses_memory_dir_when_provided() {
        let env = MockEnv::default();
        let dir = std::env::temp_dir();
        let path = resolve_inspection_log_path(Source::WebServer, Some(&dir), &env).unwrap();
        assert!(
            path.starts_with(&dir),
            "expected web-server log under memory_dir, got {}",
            path.display()
        );
    }

    #[test]
    fn cli_inspection_log_ignores_memory_dir() {
        let env = MockEnv::default();
        let dir = std::env::temp_dir();
        let path = resolve_inspection_log_path(Source::Cli, Some(&dir), &env).unwrap();
        // CLI path always rooted in $HOME or cwd, not memory_dir.
        assert!(
            !path.starts_with(&dir),
            "CLI inspection log should not live under memory_dir"
        );
    }

    #[test]
    fn build_config_honors_env_endpoint() {
        let env = MockEnv::default()
            .set("AURA_TELEMETRY_ENDPOINT", "https://posthog.example/")
            .set("AURA_TELEMETRY_API_KEY", "phc_unit_test");
        let dir = tempfile::tempdir().unwrap();
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), None, &env);
        assert_eq!(cfg.endpoint, "https://posthog.example/");
        assert_eq!(cfg.api_key, "phc_unit_test");
        assert_eq!(cfg.deployment_method, DeploymentMethod::Local);
        // Audit field is populated even when no env override fired.
        assert!(cfg.install_id_path.is_some());
    }

    #[test]
    fn build_config_picks_up_deployment_method() {
        let env = MockEnv::default().set("AURA_DEPLOYMENT_METHOD", "k8s");
        let dir = tempfile::tempdir().unwrap();
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), None, &env);
        assert_eq!(cfg.deployment_method, DeploymentMethod::K8s);
    }

    #[test]
    fn build_config_records_disabled_state_from_env() {
        let env = MockEnv::default().set("DO_NOT_TRACK", "1");
        let dir = tempfile::tempdir().unwrap();
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), None, &env);
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
        let env = MockEnv::default();
        let dir = tempfile::tempdir().unwrap();
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), None, &env);
        assert!(matches!(cfg.state, TelemetryState::Unknown));
        assert!(cfg.install_id_path.is_some());
    }

    #[test]
    fn file_enabled_false_records_config_disabled_when_env_silent() {
        let env = MockEnv::default();
        let dir = tempfile::tempdir().unwrap();
        let file = FileTelemetryConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), Some(&file), &env);
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
        let dir = tempfile::tempdir().unwrap();
        let file = FileTelemetryConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), Some(&file), &env);
        assert!(matches!(
            cfg.state,
            TelemetryState::Disabled(DisableReason::DoNotTrack)
        ));
    }

    #[test]
    fn file_endpoint_used_when_env_unset() {
        let env = MockEnv::default();
        let dir = tempfile::tempdir().unwrap();
        let file = FileTelemetryConfig {
            endpoint: Some("https://self-hosted.example/posthog".into()),
            api_key: Some("phc_self".into()),
            ..Default::default()
        };
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), Some(&file), &env);
        assert_eq!(cfg.endpoint, "https://self-hosted.example/posthog");
        assert_eq!(cfg.api_key, "phc_self");
    }

    #[test]
    fn env_endpoint_outranks_file_endpoint() {
        let env = MockEnv::default().set("AURA_TELEMETRY_ENDPOINT", "https://env-wins.example/");
        let dir = tempfile::tempdir().unwrap();
        let file = FileTelemetryConfig {
            endpoint: Some("https://file-loses.example/".into()),
            ..Default::default()
        };
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), Some(&file), &env);
        assert_eq!(cfg.endpoint, "https://env-wins.example/");
    }

    #[test]
    fn file_enabled_true_is_explicit_enable() {
        // Under the consent model `enabled = true` is the operator's
        // explicit opt-in → Enabled (not a no-op as in the old opt-out
        // model where the default was already on).
        let env = MockEnv::default();
        let dir = tempfile::tempdir().unwrap();
        let file = FileTelemetryConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), Some(&file), &env);
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
        let resolved = resolve_install_id_path(Source::Cli, None, &env);
        assert!(
            resolved.starts_with(fake_home.path()),
            "install-id path must root under the injected HOME, got {}",
            resolved.display()
        );
        assert!(resolved.ends_with(".aura/install-id"));
    }

    #[test]
    fn install_id_path_falls_back_to_memory_dir_when_home_unset() {
        let memory = tempfile::tempdir().unwrap();
        let env = MockEnv::default(); // no HOME
        let resolved = resolve_install_id_path(Source::Cli, Some(memory.path()), &env);
        assert_eq!(resolved, memory.path().join("install-id"));
    }

    /// The server's durable location is the mounted `memory_dir`, so it
    /// wins over `$HOME` (the ephemeral container layer) — mirroring the
    /// inspection-log resolution. Regression for the bug where a server
    /// with `$HOME` set wrote its install-id outside the persisted volume
    /// and silently churned the install count.
    #[test]
    fn web_server_install_id_prefers_memory_dir_over_home() {
        let memory = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", "/home/aura");
        let resolved = resolve_install_id_path(Source::WebServer, Some(memory.path()), &env);
        assert_eq!(resolved, memory.path().join("install-id"));
    }

    #[test]
    fn web_server_install_id_falls_back_to_home_without_memory_dir() {
        let env = MockEnv::default().set("HOME", "/home/aura");
        let resolved = resolve_install_id_path(Source::WebServer, None, &env);
        assert_eq!(
            resolved,
            std::path::PathBuf::from("/home/aura")
                .join(".aura")
                .join("install-id")
        );
    }

    /// End-to-end through `build_config_with_env`: even with `$HOME` set
    /// (the typical container case), a server with a configured
    /// `memory_dir` roots its install-id under that mounted volume, so it
    /// survives container recreation.
    #[test]
    fn web_server_build_config_roots_install_id_in_memory_dir() {
        let dir = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", "/home/aura");
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), None, &env);
        assert_eq!(
            cfg.install_id_path.as_deref(),
            Some(dir.path().join("install-id").as_path())
        );
    }

    #[test]
    fn install_id_durable_with_home() {
        let env = MockEnv::default().set("HOME", "/home/dev");
        assert!(install_id_is_durable(Source::Cli, None, &env));
    }

    #[test]
    fn install_id_durable_with_memory_dir_and_no_home() {
        let env = MockEnv::default();
        let dir = tempfile::tempdir().unwrap();
        assert!(install_id_is_durable(Source::Cli, Some(dir.path()), &env));
    }

    #[test]
    fn install_id_not_durable_with_neither() {
        // No HOME, no memory_dir → bare-cwd fallback → not durable.
        let env = MockEnv::default();
        assert!(!install_id_is_durable(Source::Cli, None, &env));
    }

    #[test]
    fn install_id_not_durable_with_empty_home() {
        let env = MockEnv::default().set("HOME", "");
        assert!(!install_id_is_durable(Source::Cli, None, &env));
    }

    /// Server durability hinges on `memory_dir`, not `$HOME`: a server
    /// relying on its container `$HOME` must be flagged not-durable so the
    /// warning fires, while a configured `memory_dir` is durable.
    #[test]
    fn web_server_durability_requires_memory_dir() {
        let with_home = MockEnv::default().set("HOME", "/home/aura");
        assert!(
            !install_id_is_durable(Source::WebServer, None, &with_home),
            "server with only $HOME is not durable (warning should fire)"
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(
            install_id_is_durable(Source::WebServer, Some(dir.path()), &with_home),
            "server with a mounted memory_dir is durable"
        );
    }

    /// Lock the **CLI** durability predicate against the path resolver so
    /// the two can't drift: whenever `install_id_is_durable` is true, the
    /// resolved path must NOT be the bare-cwd fallback, and vice versa.
    /// (The server predicate intentionally diverges — `$HOME` resolves a
    /// path but is not durable — so it is covered separately above.)
    #[test]
    fn install_id_durability_matches_path() {
        let cwd_fallback = std::path::PathBuf::from(".aura").join("install-id");
        let dir = tempfile::tempdir().unwrap();
        let cases: &[(Option<&std::path::Path>, Option<&str>)] = &[
            (None, Some("/home/dev")),      // HOME set
            (Some(dir.path()), None),       // memory_dir set
            (Some(dir.path()), Some("/h")), // both
            (None, None),                   // neither → cwd fallback
            (None, Some("")),               // empty HOME → cwd fallback
        ];
        for (memory_dir, home) in cases {
            let mut env = MockEnv::default();
            if let Some(h) = home {
                env = env.set("HOME", h);
            }
            let durable = install_id_is_durable(Source::Cli, *memory_dir, &env);
            let path = resolve_install_id_path(Source::Cli, *memory_dir, &env);
            assert_eq!(
                durable,
                path != cwd_fallback,
                "durability/path disagree for memory_dir={memory_dir:?} home={home:?}"
            );
        }
    }

    #[test]
    fn install_id_path_treats_empty_home_as_unset() {
        let memory = tempfile::tempdir().unwrap();
        let env = MockEnv::default().set("HOME", ""); // HOME present but empty
        let resolved = resolve_install_id_path(Source::Cli, Some(memory.path()), &env);
        // Falls through to memory_dir branch, not the literal empty
        // prefix. Without this guard `PathBuf::from("").join(".aura")`
        // would produce a relative path the caller didn't ask for.
        assert_eq!(resolved, memory.path().join("install-id"));
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
        let cfg = build_config_with_env(Source::Cli, None, None, &env);
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
