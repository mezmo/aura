//! Admission backends behind [`crate::TurnAdmission`]. `LocalAdmission`
//! serves single-instance deployments (and the `off` mode);
//! `ClaimFileAdmission` serves multi-instance deployments against a
//! shared Archil disk. Constructed only through
//! [`crate::build_admission`], which shares one arbiter and one instance
//! identity across the process. The acting instance is always the
//! backend's own — callers never supply it.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::arbiter::SessionArbiter;
use crate::claim::{HolderView, ObservedClaim};
use crate::identity::{InstanceId, SessionId};
use crate::state::{AdmissionError, HeldLock, IdleRequest};
use crate::{AdmissionEnv, TurnAdmission};

/// Why a claim read failed. Internal: the public port maps `InvalidWire`
/// to `io::ErrorKind::InvalidData` with the [`crate::WireError`] source
/// preserved (a contract Layer-2 tests pin).
#[derive(Debug)]
pub(crate) enum ObserveError {
    /// Filesystem failure.
    Io(std::io::Error),
    /// The claim file exists but its content does not parse.
    InvalidWire(crate::claim::WireError),
}

/// Arbiter-only admission (`AURA_SESSION_ADMISSION=off`, the default).
/// Same-instance requests still serialize; there is no cross-instance
/// claim. Leases are static (nothing to renew).
#[derive(Debug)]
pub struct LocalAdmission {
    instance: InstanceId,
    arbiter: SessionArbiter,
    retry_after: std::time::Duration,
}

impl LocalAdmission {
    pub(crate) fn new(instance: InstanceId, arbiter: SessionArbiter, env: &AdmissionEnv) -> Self {
        Self {
            instance,
            arbiter,
            retry_after: env.retry_after(),
        }
    }
}

#[async_trait]
impl TurnAdmission for LocalAdmission {
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError> {
        todo!(
            "fill: arbiter PendingGuard → confirm() → AcquiredClaim \
             into_held_local; aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        todo!("fill: arbiter holds() (Held only) → Here view; aura #421 follow-up")
    }
}

/// Claim-file admission (`AURA_SESSION_ADMISSION=lockfile`) against a
/// claim root the server derives from its memory dir. One well-known
/// `CLAIM` file per session is the election; heartbeats advance a
/// sequence in its body; stale claims are superseded only by the
/// evidence-gated steal, internal to [`admit`](TurnAdmission::admit) and
/// revalidated against a fresh read at steal time.
#[derive(Debug)]
pub struct ClaimFileAdmission {
    root: PathBuf,
    instance: InstanceId,
    arbiter: SessionArbiter,
    env: AdmissionEnv,
}

impl ClaimFileAdmission {
    pub(crate) fn new(
        root: PathBuf,
        instance: InstanceId,
        arbiter: SessionArbiter,
        env: AdmissionEnv,
    ) -> Self {
        Self {
            root,
            instance,
            arbiter,
            env,
        }
    }

    /// The claim root this backend manages (crate-internal: backends are
    /// reachable only through the factory's trait object, so a public
    /// accessor would be dead surface).
    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// One uncached read of a session's claim (adapter-internal; the
    /// sampler and `locate_holder` share it).
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn observe(&self, session: &SessionId) -> Result<Option<ObservedClaim>, ObserveError> {
        todo!("fill: uncached claim read; aura #421 follow-up")
    }

    /// Assemble the release action bound to one claim (adapter-internal;
    /// consumed by admit's construction of [`AcquiredClaim`]).
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    fn release_for(&self, claim: crate::claim::Generation) -> crate::state::ReleaseAction {
        todo!("fill: tombstone rename bound to claim; aura #421 follow-up")
    }
}

#[async_trait]
impl TurnAdmission for ClaimFileAdmission {
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError> {
        todo!(
            "fill: arbiter PendingGuard → O_EXCL election or evidence \
             steal (internal, revalidated) → confirm() → AcquiredClaim \
             into_held_with_actor; aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        todo!("fill: observe → Remote view; aura #421 follow-up")
    }
}
