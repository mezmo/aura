//! Deployment configuration for session admission. Read once from the
//! environment at startup; invalid values are config errors, never
//! silent clamps. Every timing knob is configurable (ruling 2026-08-23).

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::lease::{BeatInterval, LeaseTtl, SelfFenceMargin};

/// Which admission backend a deployment runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AdmissionMode {
    /// Arbiter-only: single-instance behavior (default).
    #[default]
    Off,
    /// Postgres-fenced claims: multi-instance admission.
    Pg,
}

impl fmt::Display for AdmissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AdmissionMode::Off => "off",
            AdmissionMode::Pg => "pg",
        })
    }
}

impl FromStr for AdmissionMode {
    type Err = AdmissionConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(AdmissionMode::Off),
            "pg" | "postgres" => Ok(AdmissionMode::Pg),
            other => Err(AdmissionConfigError(format!(
                "unknown session admission mode '{other}' (expected 'off' or 'pg')"
            ))),
        }
    }
}

/// Which repair-lane implementation serves Archil cures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RepairLaneKind {
    /// Prefer the archil CLI when the binary is discoverable, else the
    /// S3-API lane.
    #[default]
    Auto,
    /// The archil CLI (`invalidate-cache`, `checkout -f` + checkin).
    Cli,
    /// The S3-compatible API lane (CSI pods without the CLI).
    S3Api,
}

impl fmt::Display for RepairLaneKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RepairLaneKind::Auto => "auto",
            RepairLaneKind::Cli => "cli",
            RepairLaneKind::S3Api => "s3api",
        })
    }
}

impl FromStr for RepairLaneKind {
    type Err = AdmissionConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(RepairLaneKind::Auto),
            "cli" => Ok(RepairLaneKind::Cli),
            "s3api" | "s3-api" | "s3" => Ok(RepairLaneKind::S3Api),
            other => Err(AdmissionConfigError(format!(
                "unknown repair lane '{other}' (expected 'auto', 'cli', or 's3api')"
            ))),
        }
    }
}

/// A Postgres connection URL. Validated at config time (scheme must be
/// `postgres://` or `postgresql://`); never a bare string past the
/// config boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgUrl(String);

impl PgUrl {
    /// Parse and constrain a connection URL. The sole constructor.
    ///
    /// # Errors
    /// [`AdmissionConfigError`] when the scheme is not a Postgres scheme
    /// or the URL is empty.
    pub fn parse(raw: &str) -> Result<Self, AdmissionConfigError> {
        if raw.is_empty() {
            return Err(AdmissionConfigError(
                "AURA_SESSION_ADMISSION_PG_URL must not be empty".to_string(),
            ));
        }
        if !(raw.starts_with("postgres://") || raw.starts_with("postgresql://")) {
            return Err(AdmissionConfigError(format!(
                "AURA_SESSION_ADMISSION_PG_URL must use the postgres:// or postgresql:// scheme"
            )));
        }
        Ok(Self(raw.to_string()))
    }
}

impl AsRef<str> for PgUrl {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PgUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error reading the `AURA_SESSION_ADMISSION*` environment.
#[derive(Debug, thiserror::Error)]
#[error("invalid session admission config: {0}")]
pub struct AdmissionConfigError(pub(crate) String);

const DEFAULT_BEAT_INTERVAL_MILLIS: u64 = 5_000;
const DEFAULT_RETRY_AFTER_MILLIS: u64 = 1_000;
const DEFAULT_LEASE_TTL_MILLIS: u64 = 15_000;
const DEFAULT_FENCE_MARGIN_MILLIS: u64 = 250;
const DEFAULT_PROPAGATION_WINDOW_MILLIS: u64 = 30_000;

/// Raw string values read from the environment, before validation. Kept
/// separate from [`AdmissionEnv`] so every validation rule is reachable
/// without touching the process environment (which is forbidden
/// crate-wide by `forbid(unsafe_code)`).
pub(crate) struct AdmissionEnvValues<'a> {
    pub(crate) mode: Option<&'a str>,
    pub(crate) pg_url: Option<&'a str>,
    pub(crate) beat_interval_ms: Option<&'a str>,
    pub(crate) lease_ttl_ms: Option<&'a str>,
    pub(crate) fence_margin_ms: Option<&'a str>,
    pub(crate) retry_after_ms: Option<&'a str>,
    pub(crate) propagation_window_ms: Option<&'a str>,
    pub(crate) repair_lane: Option<&'a str>,
}

/// Effective admission configuration. Private fields: the beat interval
/// and lease ttl are non-zero by construction, and the PG-only fields
/// are reachable only through the [`PgAdmissionEnv`] narrowing proof, so
/// a local-mode build can never observe a half-populated pg config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionEnv {
    mode: AdmissionMode,
    beat: BeatInterval,
    retry_after: Duration,
    lease_ttl: LeaseTtl,
    fence_margin: SelfFenceMargin,
    propagation_window: Duration,
    pg_url: Option<PgUrl>,
    repair: RepairLaneKind,
}

/// Proof that an [`AdmissionEnv`] selected the local backend.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalAdmissionEnv<'a>(&'a AdmissionEnv);

/// Proof that an [`AdmissionEnv`] selected the Postgres backend *and*
/// carries a connection URL (the narrowing checks both, so the proof
/// cannot wrap a url-less pg config).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PgAdmissionEnv<'a>(&'a AdmissionEnv);

impl AdmissionEnv {
    /// Read the admission environment:
    ///
    /// | Env var | Meaning |
    /// | ------- | ------- |
    /// | `AURA_SESSION_ADMISSION` | `off` (default) or `pg` |
    /// | `AURA_SESSION_ADMISSION_PG_URL` | connection URL (required in `pg` mode) |
    /// | `AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS` | heartbeat interval (default 5000) |
    /// | `AURA_SESSION_ADMISSION_LEASE_TTL_MS` | server-side lease ttl (default 15000) |
    /// | `AURA_SESSION_ADMISSION_FENCE_MARGIN_MS` | self-fence margin (default 250) |
    /// | `AURA_SESSION_ADMISSION_RETRY_AFTER_MS` | `Busy` retry hint (default 1000) |
    /// | `AURA_SESSION_ADMISSION_PROPAGATION_WINDOW_MS` | read-miss retry window (default 30000; uncalibrated — soak test) |
    /// | `AURA_SESSION_REPAIR_LANE` | `auto` (default), `cli`, or `s3api` |
    ///
    /// Validation rules: every duration non-zero; `fence_margin <
    /// lease_ttl`; `pg` mode requires the URL. Defaults to `off` when
    /// unset.
    pub fn from_env() -> Result<Self, AdmissionConfigError> {
        Self::from_values(AdmissionEnvValues {
            mode: std::env::var("AURA_SESSION_ADMISSION").ok().as_deref(),
            pg_url: std::env::var("AURA_SESSION_ADMISSION_PG_URL")
                .ok()
                .as_deref(),
            beat_interval_ms: std::env::var("AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS")
                .ok()
                .as_deref(),
            lease_ttl_ms: std::env::var("AURA_SESSION_ADMISSION_LEASE_TTL_MS")
                .ok()
                .as_deref(),
            fence_margin_ms: std::env::var("AURA_SESSION_ADMISSION_FENCE_MARGIN_MS")
                .ok()
                .as_deref(),
            retry_after_ms: std::env::var("AURA_SESSION_ADMISSION_RETRY_AFTER_MS")
                .ok()
                .as_deref(),
            propagation_window_ms: std::env::var("AURA_SESSION_ADMISSION_PROPAGATION_WINDOW_MS")
                .ok()
                .as_deref(),
            repair_lane: std::env::var("AURA_SESSION_REPAIR_LANE").ok().as_deref(),
        })
    }

    /// Validate the raw environment values and assemble the effective
    /// config. All validation lives here so it is reachable without
    /// mutating the process environment (forbidden crate-wide).
    pub(crate) fn from_values(vals: AdmissionEnvValues<'_>) -> Result<Self, AdmissionConfigError> {
        let mode = match vals.mode {
            Some(s) => AdmissionMode::from_str(s)?,
            None => AdmissionMode::Off,
        };

        let pg_url = match mode {
            AdmissionMode::Pg => {
                let raw = vals.pg_url.ok_or_else(|| {
                    AdmissionConfigError(
                        "AURA_SESSION_ADMISSION_PG_URL is required in pg mode".to_string(),
                    )
                })?;
                Some(PgUrl::parse(raw)?)
            }
            // Absent-or-ignored cleanly in off mode: a stray URL never
            // fails a local deployment.
            AdmissionMode::Off => None,
        };

        let beat_millis = parse_millis(
            "AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS",
            vals.beat_interval_ms,
            DEFAULT_BEAT_INTERVAL_MILLIS,
        )?;
        let beat = BeatInterval::new(Duration::from_millis(beat_millis)).map_err(|_| {
            AdmissionConfigError(
                "AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS must be a positive number of milliseconds"
                    .to_string(),
            )
        })?;

        let lease_millis = parse_millis(
            "AURA_SESSION_ADMISSION_LEASE_TTL_MS",
            vals.lease_ttl_ms,
            DEFAULT_LEASE_TTL_MILLIS,
        )?;
        let lease_ttl = LeaseTtl::new(Duration::from_millis(lease_millis)).map_err(|_| {
            AdmissionConfigError(
                "AURA_SESSION_ADMISSION_LEASE_TTL_MS must be a positive number of milliseconds"
                    .to_string(),
            )
        })?;

        let margin_millis = parse_millis(
            "AURA_SESSION_ADMISSION_FENCE_MARGIN_MS",
            vals.fence_margin_ms,
            DEFAULT_FENCE_MARGIN_MILLIS,
        )?;
        let fence_margin = SelfFenceMargin::new(Duration::from_millis(margin_millis)).map_err(
            |_| {
                AdmissionConfigError(
                    "AURA_SESSION_ADMISSION_FENCE_MARGIN_MS must be a positive number of milliseconds"
                    .to_string(),
                )
            },
        )?;

        if fence_margin.get() >= lease_ttl.get() {
            return Err(AdmissionConfigError(format!(
                "AURA_SESSION_ADMISSION_FENCE_MARGIN_MS ({margin_millis}) must be less than AURA_SESSION_ADMISSION_LEASE_TTL_MS ({lease_millis})"
            )));
        }

        let retry_millis = parse_millis(
            "AURA_SESSION_ADMISSION_RETRY_AFTER_MS",
            vals.retry_after_ms,
            DEFAULT_RETRY_AFTER_MILLIS,
        )?;
        if retry_millis == 0 {
            return Err(AdmissionConfigError(
                "AURA_SESSION_ADMISSION_RETRY_AFTER_MS must be a positive number of milliseconds"
                    .to_string(),
            ));
        }
        let retry_after = Duration::from_millis(retry_millis);

        let window_millis = parse_millis(
            "AURA_SESSION_ADMISSION_PROPAGATION_WINDOW_MS",
            vals.propagation_window_ms,
            DEFAULT_PROPAGATION_WINDOW_MILLIS,
        )?;
        if window_millis == 0 {
            return Err(AdmissionConfigError(
                "AURA_SESSION_ADMISSION_PROPAGATION_WINDOW_MS must be a positive number of milliseconds"
                    .to_string(),
            ));
        }
        let propagation_window = Duration::from_millis(window_millis);

        let repair = match vals.repair_lane {
            Some(s) => RepairLaneKind::from_str(s)?,
            None => RepairLaneKind::Auto,
        };

        Ok(Self {
            mode,
            beat,
            retry_after,
            lease_ttl,
            fence_margin,
            propagation_window,
            pg_url,
            repair,
        })
    }

    /// The configured backend mode.
    #[must_use]
    pub const fn mode(&self) -> AdmissionMode {
        self.mode
    }

    /// The heartbeat interval.
    #[must_use]
    pub const fn beat(&self) -> BeatInterval {
        self.beat
    }

    /// The `Busy` retry hint.
    #[must_use]
    pub const fn retry_after(&self) -> Duration {
        self.retry_after
    }

    /// The server-side lease ttl.
    #[must_use]
    pub const fn lease_ttl(&self) -> LeaseTtl {
        self.lease_ttl
    }

    /// The self-fence margin.
    #[must_use]
    pub const fn fence_margin(&self) -> SelfFenceMargin {
        self.fence_margin
    }

    /// The read-miss propagation window (the aura seam's retry budget
    /// before escalating to the repair lane).
    #[must_use]
    pub const fn propagation_window(&self) -> Duration {
        self.propagation_window
    }

    /// The configured repair lane kind.
    #[must_use]
    pub const fn repair_lane(&self) -> RepairLaneKind {
        self.repair
    }

    /// Narrow this config to the local backend.
    #[must_use]
    pub(crate) fn local(&self) -> Option<LocalAdmissionEnv<'_>> {
        matches!(self.mode, AdmissionMode::Off).then_some(LocalAdmissionEnv(self))
    }

    /// Narrow this config to the Postgres backend. Returns `None` unless
    /// the mode is `Pg` *and* a URL is present, so the proof is
    /// unforgeable for a half-configured pg mode.
    #[must_use]
    pub(crate) fn pg(&self) -> Option<PgAdmissionEnv<'_>> {
        if matches!(self.mode, AdmissionMode::Pg) && self.pg_url.is_some() {
            Some(PgAdmissionEnv(self))
        } else {
            None
        }
    }
}

impl LocalAdmissionEnv<'_> {
    /// The `Busy` retry hint.
    #[must_use]
    pub(crate) const fn retry_after(self) -> Duration {
        self.0.retry_after
    }

    /// The read-miss propagation window (uniform read path in both
    /// modes).
    #[must_use]
    pub(crate) const fn propagation_window(self) -> Duration {
        self.0.propagation_window
    }
}

impl PgAdmissionEnv<'_> {
    /// The connection URL (presence pinned by the narrowing).
    #[must_use]
    pub(crate) fn pg_url(self) -> PgUrl {
        self.0
            .pg_url
            .clone()
            .expect("PgAdmissionEnv: URL presence pinned by AdmissionEnv::pg")
    }

    /// The heartbeat interval (the heartbeat lease cannot be assembled
    /// without it).
    #[must_use]
    pub(crate) const fn beat(self) -> BeatInterval {
        self.0.beat
    }

    /// The server-side lease ttl.
    #[must_use]
    pub(crate) const fn lease_ttl(self) -> LeaseTtl {
        self.0.lease_ttl
    }

    /// The self-fence margin.
    #[must_use]
    pub(crate) const fn fence_margin(self) -> SelfFenceMargin {
        self.0.fence_margin
    }

    /// The `Busy` retry hint.
    #[must_use]
    pub(crate) const fn retry_after(self) -> Duration {
        self.0.retry_after
    }

    /// The read-miss propagation window.
    #[must_use]
    pub(crate) const fn propagation_window(self) -> Duration {
        self.0.propagation_window
    }

    /// The configured repair lane kind.
    #[must_use]
    pub(crate) const fn repair_lane(self) -> RepairLaneKind {
        self.0.repair
    }
}

/// Parse a millisecond knob: absent uses `default`, present must be a
/// `u64` (zero is rejected downstream by the non-zero constructors or
/// the positivity checks).
fn parse_millis(name: &str, raw: Option<&str>, default: u64) -> Result<u64, AdmissionConfigError> {
    match raw {
        None => Ok(default),
        Some(s) => s.parse::<u64>().map_err(|_| {
            AdmissionConfigError(format!(
                "{name} must be a positive integer number of milliseconds, got '{s}'"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(
        mode: Option<&'static str>,
        pg_url: Option<&'static str>,
        beat: Option<&'static str>,
        lease: Option<&'static str>,
        margin: Option<&'static str>,
        retry: Option<&'static str>,
        window: Option<&'static str>,
        repair: Option<&'static str>,
    ) -> AdmissionEnvValues<'static> {
        AdmissionEnvValues {
            mode,
            pg_url,
            beat_interval_ms: beat,
            lease_ttl_ms: lease,
            fence_margin_ms: margin,
            retry_after_ms: retry,
            propagation_window_ms: window,
            repair_lane: repair,
        }
    }

    fn defaults() -> AdmissionEnvValues<'static> {
        values(None, None, None, None, None, None, None, None)
    }

    #[test]
    fn off_defaults() {
        let env = AdmissionEnv::from_values(defaults()).expect("off defaults are valid");
        assert_eq!(env.mode(), AdmissionMode::Off);
        assert_eq!(env.beat().get(), Duration::from_millis(5_000));
        assert_eq!(env.retry_after(), Duration::from_millis(1_000));
        assert_eq!(env.lease_ttl().get(), Duration::from_millis(15_000));
        assert_eq!(env.fence_margin().get(), Duration::from_millis(250));
        assert_eq!(env.propagation_window(), Duration::from_millis(30_000));
        assert_eq!(env.repair_lane(), RepairLaneKind::Auto);
        assert!(env.local().is_some());
        assert!(env.pg().is_none());
    }

    #[test]
    fn pg_without_url_fails() {
        let err =
            AdmissionEnv::from_values(values(Some("pg"), None, None, None, None, None, None, None))
                .expect_err("pg mode without a URL fails loud");
        assert!(!err.0.is_empty());
    }

    #[test]
    fn pg_with_bad_scheme_fails() {
        let err = AdmissionEnv::from_values(values(
            Some("pg"),
            Some("redis://://x"),
            None,
            None,
            None,
            None,
            None,
            None,
        ))
        .expect_err("pg mode with a non-postgres scheme fails");
        assert!(!err.0.is_empty());
    }

    #[test]
    fn margin_gte_tt_fails() {
        let err = AdmissionEnv::from_values(values(
            Some("pg"),
            Some("postgres://x"),
            None,
            Some("100"),
            Some("100"),
            None,
            None,
            None,
        ))
        .expect_err("margin equal to ttl fails");
        assert!(!err.0.is_empty());
    }

    #[test]
    fn zero_duration_fails() {
        let err =
            AdmissionEnv::from_values(values(None, None, Some("0"), None, None, None, None, None))
                .expect_err("zero beat interval fails");
        assert!(!err.0.is_empty());
    }

    #[test]
    fn non_numeric_millis_fails() {
        let err = AdmissionEnv::from_values(values(
            None,
            None,
            Some("abc"),
            None,
            None,
            None,
            None,
            None,
        ))
        .expect_err("non-numeric millis fails");
        assert!(!err.0.is_empty());
    }

    #[test]
    fn unknown_mode_fails() {
        let err = AdmissionEnv::from_values(values(
            Some("bogus"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ))
        .expect_err("unknown mode fails");
        assert!(!err.0.is_empty());
    }

    #[test]
    fn repair_lane_parsing() {
        let cli = AdmissionEnv::from_values(values(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("cli"),
        ))
        .expect("cli lane parses");
        assert_eq!(cli.repair_lane(), RepairLaneKind::Cli);

        let s3 = AdmissionEnv::from_values(values(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("s3api"),
        ))
        .expect("s3api lane parses");
        assert_eq!(s3.repair_lane(), RepairLaneKind::S3Api);

        let err = AdmissionEnv::from_values(values(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("nope"),
        ))
        .expect_err("unknown lane fails");
        assert!(!err.0.is_empty());
    }
}
