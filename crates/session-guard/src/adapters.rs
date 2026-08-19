//! Admission backends behind [`crate::TurnAdmission`]. `LocalAdmission`
//! serves single-instance deployments (and the `off` mode);
//! `ClaimFileAdmission` serves multi-instance deployments against a
//! shared Archil disk. Constructed only through
//! [`crate::build_admission`], which shares one arbiter across the
//! process.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::arbiter::SessionArbiter;
use crate::claim::{HolderView, ObservedClaim};
use crate::identity::{InstanceId, SessionId};
use crate::state::{AdmissionError, HeldLock, IdleRequest};
use crate::{AdmissionEnv, TurnAdmission};

/// Arbiter-only admission (`AURA_SESSION_ADMISSION=off`, the default).
/// Same-instance requests still serialize; there is no cross-instance
/// claim. Leases are static (nothing to renew).
#[derive(Debug)]
pub struct LocalAdmission {
    arbiter: SessionArbiter,
    retry_after: std::time::Duration,
}

impl LocalAdmission {
    pub(crate) fn new(arbiter: SessionArbiter, env: &AdmissionEnv) -> Self {
        Self {
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
        todo!("fill: arbiter hold + static lease + no-op release; aura #421 follow-up")
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        todo!("fill: report Here when the arbiter holds it; aura #421 follow-up")
    }
}

/// Claim-file admission (`AURA_SESSION_ADMISSION=lockfile`) against a
/// claim root the server derives from its memory dir. One well-known
/// `CLAIM` file per session is the election; heartbeats advance a
/// sequence in its body; stale claims are superseded only by the
/// evidence-gated steal, internal to [`admit`](TurnAdmission::admit).
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

    /// The claim root this backend manages.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// One uncached read of a session's claim (adapter-internal; the
    /// sampler and `locate_holder` share it).
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn observe(&self, session: &SessionId) -> std::io::Result<Option<ObservedClaim>> {
        todo!("fill: uncached claim read; aura #421 follow-up")
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
            "fill: arbiter → O_EXCL election or evidence steal (internal) + \
             lease actor; aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        todo!("fill: observe → HolderView; aura #421 follow-up")
    }
}
