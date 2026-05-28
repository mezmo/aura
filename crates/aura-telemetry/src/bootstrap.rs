//! Default-resolution helpers for `aura-web-server` and `aura-cli`.
//!
//! These exist so the two integration sites do not drift in how they
//! resolve env vars, default paths, or the kill-switch decision. The
//! [`build_config_from_env`] function returns a fully populated
//! [`TelemetryConfig`] given a `Source` and an optional `memory_dir`;
//! callers pass it straight into [`crate::init`].
//!
//! Anything env-driven goes through this module so a future audit of
//! "which env vars affect telemetry" only needs to grep here.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::disable::{decide_disabled, DisableReason, EnvProvider, SystemEnv};
use crate::install_id;
use crate::properties::{DeploymentMethod, OsFamily, Source};
use crate::TelemetryConfig;

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

/// Resolve the install-id persistence path.
///
/// Preference order:
/// 1. `$HOME/.aura/install-id` if `HOME` is set (the path users see
///    documented in `docs/telemetry.md`).
/// 2. `{memory_dir}/install-id` when supplied by the server (system
///    accounts without `$HOME`).
/// 3. `.aura/install-id` relative to the current working directory as
///    a last resort.
pub fn resolve_install_id_path(memory_dir: Option<&Path>) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".aura").join("install-id");
    }
    if let Some(dir) = memory_dir {
        return dir.join("install-id");
    }
    PathBuf::from(".aura").join("install-id")
}

/// Resolve the local inspection-log path. Returns `None` when the user
/// has opted out of the inspection log via `AURA_TELEMETRY_LOG_EVENTS=0`.
pub fn resolve_inspection_log_path(
    source: Source,
    memory_dir: Option<&Path>,
    env: &dyn EnvProvider,
) -> Option<PathBuf> {
    if env.var("AURA_TELEMETRY_LOG_EVENTS").as_deref() == Some("0") {
        return None;
    }
    Some(default_inspection_log_path(source, memory_dir))
}

fn default_inspection_log_path(source: Source, memory_dir: Option<&Path>) -> PathBuf {
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
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".aura")
            .join("telemetry")
            .join("events.jsonl");
    }
    PathBuf::from(".aura")
        .join("telemetry")
        .join("events.jsonl")
}

/// Resolve a [`TelemetryConfig`] from the environment + (optional)
/// `memory_dir` + the disable decision tree. The returned config is
/// ready to pass to [`crate::init`].
///
/// `install_id` is created (or read) at the resolved path; an error
/// here is non-fatal — we log at `tracing::debug!` and return a config
/// with a freshly-generated UUID that will not persist. This keeps
/// telemetry init from ever blocking startup on a filesystem hiccup.
pub fn build_config_from_env(source: Source, memory_dir: Option<&Path>) -> TelemetryConfig {
    let env = SystemEnv;
    build_config_with_env(source, memory_dir, &env)
}

/// Same as [`build_config_from_env`] but takes an explicit env provider
/// for unit testability.
pub fn build_config_with_env(
    source: Source,
    memory_dir: Option<&Path>,
    env: &dyn EnvProvider,
) -> TelemetryConfig {
    let endpoint = env
        .var("AURA_TELEMETRY_ENDPOINT")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    let api_key = env
        .var("AURA_TELEMETRY_API_KEY")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API_KEY.to_string());
    let deployment_method =
        DeploymentMethod::parse(env.var("AURA_DEPLOYMENT_METHOD").as_deref());

    let install_id_path = resolve_install_id_path(memory_dir);
    let install_id = match install_id::read_or_create(&install_id_path) {
        Ok(uuid) => uuid,
        Err(e) => {
            tracing::debug!(error = %e, path = %install_id_path.display(),
                "could not persist install-id; telemetry will use a one-off UUID");
            Uuid::new_v4()
        }
    };

    let inspection_log_path = resolve_inspection_log_path(source, memory_dir, env);
    let disable_reason = decide_disabled(env);

    let mut cfg = TelemetryConfig::default_for(
        source,
        install_id,
        endpoint,
        api_key,
        inspection_log_path,
    );
    cfg.deployment_method = deployment_method;
    cfg.os_family = OsFamily::current();
    cfg.disable_reason = disable_reason;
    cfg
}

/// Best-effort label for the disable reason (or "active") suitable for
/// a single info-level log line at startup.
pub fn startup_log_line(reason: Option<&DisableReason>) -> String {
    use crate::inspection_log::disable_reason_label;
    match reason {
        None => "telemetry: active".to_string(),
        Some(r) => format!("telemetry: disabled ({})", disable_reason_label(r)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    }

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
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), &env);
        assert_eq!(cfg.endpoint, "https://posthog.example/");
        assert_eq!(cfg.api_key, "phc_unit_test");
        assert_eq!(cfg.deployment_method, DeploymentMethod::Local);
    }

    #[test]
    fn build_config_picks_up_deployment_method() {
        let env = MockEnv::default().set("AURA_DEPLOYMENT_METHOD", "k8s");
        let dir = tempfile::tempdir().unwrap();
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), &env);
        assert_eq!(cfg.deployment_method, DeploymentMethod::K8s);
    }

    #[test]
    fn build_config_records_disable_reason_from_env() {
        let env = MockEnv::default().set("DO_NOT_TRACK", "1");
        let dir = tempfile::tempdir().unwrap();
        let cfg = build_config_with_env(Source::WebServer, Some(dir.path()), &env);
        assert!(matches!(
            cfg.disable_reason,
            Some(DisableReason::DoNotTrack)
        ));
    }

    #[test]
    fn startup_log_line_active_and_disabled() {
        assert_eq!(startup_log_line(None), "telemetry: active");
        assert_eq!(
            startup_log_line(Some(&DisableReason::DoNotTrack)),
            "telemetry: disabled (DoNotTrack)"
        );
    }
}
