//! `[telemetry]` file-config types. Owned by aura-config per the
//! workspace rule that all serializable TOML config types live here;
//! the consent/runtime machinery that consumes them lives in
//! `aura-telemetry`.

/// File-driven telemetry settings as they appear under a `[telemetry]`
/// block in the main server config (`config.toml`) or the per-user
/// `cli.toml`. Every field is optional so partial configs are valid;
/// the bootstrap layer applies env > file > built-in defaults.
///
/// This struct is also where the `enabled = false` user-facing kill
/// switch documented in `docs/telemetry.md` is wired in. When a caller
/// passes a file config with `enabled = Some(false)` and no env-level
/// disable fired first, the bootstrap layer records the disable as
/// `DisableReason::ConfigDisabled`.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileTelemetryConfig {
    /// `Some(false)` → ConfigDisabled (lowest-precedence kill switch).
    /// `Some(true)` and `None` are no-ops (env-level decisions still
    /// apply, and the built-in default is on).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Override the PostHog endpoint. Env `AURA_TELEMETRY_ENDPOINT`
    /// still wins.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Override the PostHog API key. Env `AURA_TELEMETRY_API_KEY` still
    /// wins.
    #[serde(default)]
    pub api_key: Option<String>,
}
