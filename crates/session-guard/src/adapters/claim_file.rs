//! Claim-file admission (`AURA_SESSION_ADMISSION=lockfile`) against a
//! claim root the server derives from its memory dir. One well-known
//! `CLAIM` file per session is the election; heartbeats advance a
//! sequence in its body; stale claims are superseded only by the
//! evidence-gated steal, internal to [`admit`](TurnAdmission::admit) and
//! revalidated against a fresh read at steal time.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::arbiter::SessionArbiter;
use crate::claim::{HolderView, ObservedClaim};
use crate::identity::{InstanceId, SessionId};
use crate::state::{AdmissionError, HeldLock, IdleRequest};
use crate::{AdmissionEnv, TurnAdmission};

/// Proof that a claim is being assembled by the claim-file backend. The
/// constructor is private to this module, so only
/// [`ClaimFileAdmission`] can produce one — which is what pins claim-file
/// claims to heartbeat leases.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeartbeatLeaseProof(());

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

#[derive(Debug)]
pub struct ClaimFileAdmission {
    root: PathBuf,
    instance: InstanceId,
    arbiter: SessionArbiter,
    env: AdmissionEnv,
    proof: HeartbeatLeaseProof,
}

impl ClaimFileAdmission {
    pub(crate) fn new(
        root: PathBuf,
        instance: InstanceId,
        arbiter: SessionArbiter,
        env: crate::config::ClaimFileAdmissionEnv<'_>,
    ) -> Self {
        Self {
            root,
            instance,
            arbiter,
            env: env.to_owned_env(),
            proof: HeartbeatLeaseProof(()),
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
    fn release_for(&self, claim: &crate::claim::ObservedClaim) -> crate::state::ReleaseAction {
        todo!("fill: tombstone rename derived from observation; aura #421 follow-up")
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
             steal (internal, revalidated) → confirm() → \
             AcquiredClaim::new_with_heartbeat(self.proof) → into_held; \
             aura #421 follow-up"
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
