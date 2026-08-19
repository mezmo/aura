//! # session-guard
//!
//! Claim-based turn admission for multi-pod AURA: at most one pod runs a
//! turn for a given session at a time, with cross-pod memory on a shared
//! Archil disk.
//!
//! Two layers keep that promise:
//!
//! 1. **In-process** — a [`SessionArbiter`] serializes same-session requests
//!    arriving at one pod, before any filesystem admission.
//! 2. **Cross-pod** — a *claim file* on the shared disk (`locks/{session}/
//!    {generation}-{turn}.CLAIM`). Claims are additive and uniquely named, so
//!    an owner only ever removes its own claim; no client can delete another
//!    generation's claim. Liveness is a heartbeat *sequence* (never wall
//!    clocks); a claim whose sequence stops advancing for `STALE_FACTOR`
//!    beat intervals may be stolen only with [`StalenessEvidence`] built
//!    from two uncached reads at least `stale_after()` apart.
//!
//! The consuming state machine (one pod's view of one request):
//!
//! ```text
//! Idle ──admit──────────► HeldLock ──open_run──► FencedRun ──activate──► ActiveTurn
//!  │                        │                      │                       │
//!  └─busy──► 503            └─error returns lock   └─error returns lock    ├─complete──► CommittingTurn
//!                                                  (quarantine on loss)   │              │ barrier(commit)
//!                                                                         └─abort────────► └─► CommittedResponse
//! ```
//!
//! Only a [`CommittedResponse`] authorizes emitting the terminal frame of a
//! turn (SSE `[DONE]`, final body, A2A artifact). Mid-turn streaming events
//! are not gated.
//!
//! ## Configuration
//!
//! Configured **only via environment variables** — deployment infrastructure,
//! one instance per server, exactly like the session store
//! (`crates/aura-config/src/session_store.rs`):
//!
//! | Env var                                    | Meaning                                        |
//! | ------------------------------------------ | ---------------------------------------------- |
//! | `AURA_SESSION_ADMISSION`                   | `off` (default) or `lockfile`                  |
//! | `AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS`  | heartbeat interval (default 5000)              |
//! | `AURA_SESSION_ADMISSION_RETRY_AFTER_MS`    | `Busy` retry hint (default 1000)               |
//!
//! `off` builds a [`LocalAdmission`] (arbiter-only, single-instance behavior);
//! `lockfile` builds a [`ClaimFileAdmission`] against a claim root the server
//! derives from its memory dir.

#![allow(dead_code)]
// session-guard skeleton: remove slices as bodies fill (aura #421 follow-up).

mod adapters;
mod arbiter;
mod claim;
mod identity;
mod lease;
mod state;

pub use adapters::{ClaimFileAdmission, LocalAdmission};
pub use arbiter::{ArbiterGuard, SessionArbiter};
pub use claim::{
    ClaimBody, Generation, HeartbeatSample, HeartbeatSeq, HolderView, Locality, LockFingerprint,
    StalenessEvidence, claim_path,
};
pub use identity::{InstanceId, InvalidInstanceId, InvalidSessionId, SessionId, TurnId};
pub use lease::{LeaseLost, LeaseState, WriteCapability};
pub use state::{
    ActiveTurn, AdmissionError, BarrierError, CommittedResponse, CommittingTurn, FenceCause,
    FencedRun, HeldLock, IdleRequest, OpenRunError, ReleaseError, TurnOutcome,
};

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// A claim is stealable after this many missed beat intervals.
pub const STALE_FACTOR: u32 = 3;

/// The duration without an advancing heartbeat after which a claim is stale.
#[must_use]
pub fn stale_after(beat_interval: Duration) -> Duration {
    beat_interval.saturating_mul(STALE_FACTOR)
}

/// Which admission backend a deployment runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AdmissionMode {
    /// Arbiter-only: single-instance behavior (default).
    #[default]
    Off,
    /// Claim files on the shared memory dir: multi-pod admission.
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
pub struct AdmissionConfigError(String);

const DEFAULT_BEAT_INTERVAL: Duration = Duration::from_millis(5_000);
const DEFAULT_RETRY_AFTER: Duration = Duration::from_millis(1_000);

/// Effective admission configuration for one server deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionEnv {
    /// Which backend to build.
    pub mode: AdmissionMode,
    /// Heartbeat interval for held claims (drives [`stale_after`]).
    pub beat_interval: Duration,
    /// Retry hint carried by [`AdmissionError::Busy`].
    pub retry_after: Duration,
}

impl AdmissionEnv {
    /// Read the `AURA_SESSION_ADMISSION*` environment variables, defaulting
    /// to `off` when unset.
    pub fn from_env() -> Result<Self, AdmissionConfigError> {
        todo!("fill: env parsing with defaults; aura #421 follow-up")
    }
}

/// The port: everything the web server and HITL routing consume. Built at
/// startup from [`AdmissionEnv`] exactly like the session store backend.
#[async_trait::async_trait]
pub trait TurnAdmission: Send + Sync {
    /// Fresh admission (S0→S2): create a new claim, or fail [`AdmissionError::Busy`]
    /// if a live claim exists.
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError>;

    /// Evidence-gated steal (S1→S2a): replace a claim proven stale by
    /// `evidence`. The evidence binds session, owner, generation, and two
    /// non-advancing heartbeat samples; the implementation revalidates it
    /// against the claim file before replacing.
    async fn admit_with_evidence(
        &self,
        req: IdleRequest,
        evidence: StalenessEvidence,
    ) -> Result<HeldLock, AdmissionError>;

    /// Fresh read-only holder lookup for HITL routing, callable from any pod.
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>>;
}
