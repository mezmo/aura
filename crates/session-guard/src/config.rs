use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::lease::BeatInterval;

/// Which admission backend a deployment runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AdmissionMode {
    /// Arbiter-only: single-instance behavior (default).
    #[default]
    Off,
    /// Claim files on the shared memory dir: multi-instance admission.
    LockFile,
}

impl fmt::Display for AdmissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AdmissionMode::Off => "off",
            AdmissionMode::LockFile => "lockfile",
        })
    }
}

impl FromStr for AdmissionMode {
    type Err = AdmissionConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(AdmissionMode::Off),
            "lockfile" | "claim-file" => Ok(AdmissionMode::LockFile),
            other => Err(AdmissionConfigError(format!(
                "unknown session admission mode '{other}' (expected 'off' or 'lockfile')"
            ))),
        }
    }
}

/// Error reading the `AURA_SESSION_ADMISSION*` environment.
#[derive(Debug, thiserror::Error)]
#[error("invalid session admission config: {0}")]
pub struct AdmissionConfigError(pub(crate) String);

const DEFAULT_BEAT_INTERVAL_MILLIS: u64 = 5_000;
const DEFAULT_RETRY_AFTER_MILLIS: u64 = 1_000;

/// Effective admission configuration. Private fields: the beat interval
/// is non-zero by construction, so `stale_after` cannot degenerate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionEnv {
    mode: AdmissionMode,
    beat: BeatInterval,
    retry_after: Duration,
}

/// Proof that an [`AdmissionEnv`] selected the local backend.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalAdmissionEnv<'a>(&'a AdmissionEnv);

/// Proof that an [`AdmissionEnv`] selected the claim-file backend.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClaimFileAdmissionEnv<'a>(&'a AdmissionEnv);

impl AdmissionEnv {
    /// Read the `AURA_SESSION_ADMISSION*` environment variables,
    /// defaulting to `off` when unset. A zero beat interval or retry hint
    /// is a config error, not a silent clamp.
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

    /// Narrow this config to the local backend.
    #[must_use]
    pub(crate) fn local(&self) -> Option<LocalAdmissionEnv<'_>> {
        matches!(self.mode, AdmissionMode::Off).then_some(LocalAdmissionEnv(self))
    }

    /// Narrow this config to the claim-file backend.
    #[must_use]
    pub(crate) fn claim_file(&self) -> Option<ClaimFileAdmissionEnv<'_>> {
        matches!(self.mode, AdmissionMode::LockFile).then_some(ClaimFileAdmissionEnv(self))
    }
}

impl LocalAdmissionEnv<'_> {
    /// The `Busy` retry hint.
    #[must_use]
    pub(crate) const fn retry_after(self) -> Duration {
        self.0.retry_after
    }
}

impl ClaimFileAdmissionEnv<'_> {
    /// The owned config carried by claim-file admission.
    #[must_use]
    pub(crate) fn to_owned_env(self) -> AdmissionEnv {
        self.0.clone()
    }
}
