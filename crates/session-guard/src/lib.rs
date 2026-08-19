//! # session-guard
//!
//! Claim-based turn admission for multi-instance AURA: at most one
//! service instance runs a turn for a given session at a time, with
//! cross-instance memory on a shared Archil disk.
//!
//! Two layers keep that promise:
//!
//! 1. **In-process** — a session arbiter (internal) serializes
//!    same-session requests arriving at one instance, before any
//!    filesystem admission.
//! 2. **Cross-instance** — one well-known claim file per session
//!    (`{root}/{session}/CLAIM`), atomically created (`O_EXCL`): that
//!    create *is* the election. Liveness is a heartbeat *sequence* in the
//!    claim body (never wall clocks); a claim whose sequence stops
//!    advancing for the staleness window may be superseded, but only by
//!    the adapter's evidence-gated steal with revalidation.
//!
//! The consuming state machine (one instance's view of one request):
//!
//! ```text
//! Idle ──admit──────────► HeldLock ──create_run──► FencedRun ──activate──► ActiveTurn
//!  │                        │         (asserts capability, creates, rechecks;     │
//!  └─busy──► 503            │          error returns the lock for abort)          │
//!                           └─create_run Err: abort returned lock                 ├─complete──► CommittingTurn
//!                                                     ┌───────────────────────────┘              │ barrier(commit(ctx))
//!                                     abandonment ────┘                                      └─► CommittedResponse
//!                                     (drop revokes)
//! ```
//!
//! Only a [`CommittedResponse`] authorizes emitting the terminal frame of
//! a turn (SSE `[DONE]`, final body, A2A artifact). Mid-turn streaming
//! events are not gated.
//!
//! ## Configuration
//!
//! Env-configured deployment infrastructure, one instance per server,
//! exactly like the session store (`crates/aura-config/src/session_store.rs`):
//!
//! | Env var                                   | Meaning                            |
//! | ----------------------------------------- | ---------------------------------- |
//! | `AURA_SESSION_ADMISSION`                  | `off` (default) or `lockfile`      |
//! | `AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS` | heartbeat interval (default 5000)  |
//! | `AURA_SESSION_ADMISSION_RETRY_AFTER_MS`   | `Busy` retry hint (default 1000)   |
//!
//! Build the backend with [`build_admission`] — `off` yields a
//! [`LocalAdmission`] (arbiter-only), `lockfile` a
//! [`ClaimFileAdmission`] against a claim root derived from the memory
//! dir. The factory shares one internal arbiter and one instance
//! identity across the process. Build it once per server and share the
//! product: a second factory call creates a second, independent
//! arbiter, which would void same-process exclusion in `off` mode (the
//! `O_EXCL` election still backstops `lockfile` mode).

#![allow(dead_code)]
// session-guard skeleton: remove slices as bodies fill (aura #421 follow-up).

mod adapters;
mod arbiter;
mod claim;
mod identity;
mod lease;
mod state;

pub use adapters::{ClaimFileAdmission, LocalAdmission};
pub use claim::{
    Generation, HeartbeatSeq, HolderView, Locality, WireError, claim_path, tombstone_path,
};
pub use identity::{
    InstanceId, InvalidInstanceId, InvalidSessionId, InvalidTurnId, SessionId, TurnId,
};
pub use lease::{BeatInterval, LeaseLost, LeaseState, WriteCapability};
pub use state::{
    ActiveTurn, AdmissionError, BarrierError, CleanupOutcome, CommitContext, CommittedResponse,
    CommittingTurn, CreateRunError, FenceCause, FencedRun, HeldLock, IdleRequest, ReleaseError,
    TurnOutcome,
};

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// A claim is stealable after this many missed beat intervals.
pub const STALE_FACTOR: u32 = 3;

/// The duration without an advancing heartbeat after which a claim is
/// stale.
#[must_use]
pub fn stale_after(beat: BeatInterval) -> Duration {
    beat.get().saturating_mul(STALE_FACTOR)
}

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
pub struct AdmissionConfigError(String);

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
}

/// Build the deployment's admission backend from validated config: one
/// shared arbiter, one backend, selected by mode. The single factory —
/// backend constructors are crate-internal so `LocalAdmission` can never
/// be built for a `lockfile` config.
///
/// # Errors
/// [`AdmissionConfigError`] when `env` selects `lockfile` but `root` is
/// empty, or the instance id fails validation.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by aura #421 follow-up"
)]
pub fn build_admission(
    env: &AdmissionEnv,
    root: Option<PathBuf>,
    instance: InstanceId,
) -> Result<Arc<dyn TurnAdmission>, AdmissionConfigError> {
    todo!("fill: mode dispatch + shared arbiter; aura #421 follow-up")
}

/// The port: everything the web server and HITL routing consume. Steals
/// are adapter-internal (evidence assembly is not externally producible);
/// callers see them as ordinary admission outcomes.
#[async_trait::async_trait]
pub trait TurnAdmission: Send + Sync {
    /// Fresh admission (S0→S2): atomically create the session's claim, or
    /// fail [`AdmissionError::Busy`] if a live claim exists (a stale one
    /// is evidence-tested and superseded internally).
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError>;

    /// Fresh read-only holder lookup for HITL routing, callable from any
    /// instance.
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>>;
}
