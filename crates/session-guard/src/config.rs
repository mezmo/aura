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
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub fn parse(raw: &str) -> Result<Self, AdmissionConfigError> {
        todo!("fill: scheme/empty validation; aura #421 follow-up")
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
        todo!("fill: env parsing with validation; aura #421 follow-up")
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

    /// The configured repair lane kind.
    #[must_use]
    pub(crate) const fn repair_lane(self) -> RepairLaneKind {
        self.0.repair
    }
}
