//! Admission backends behind [`crate::TurnAdmission`]. `LocalAdmission`
//! serves single-instance deployments (and the `off` mode); the claim-file
//! backend serves multi-instance deployments against a shared Archil disk.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::arbiter::SessionArbiter;
use crate::claim::{HolderView, StalenessEvidence};
use crate::identity::{InstanceId, SessionId};
use crate::state::{AdmissionError, HeldLock, IdleRequest};
use crate::{AdmissionEnv, TurnAdmission};

/// Arbiter-only admission (`AURA_SESSION_ADMISSION=off`, the default).
/// Same-instance requests still serialize; there is no cross-instance
/// claim. Leases are static (nothing to renew).
#[derive(Debug, Clone)]
pub struct LocalAdmission {
    arbiter: SessionArbiter,
    env: AdmissionEnv,
}

impl LocalAdmission {
    /// Build local admission.
    #[must_use]
    pub fn new(env: AdmissionEnv) -> Self {
        Self {
            arbiter: SessionArbiter::new(),
            env,
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

    async fn admit_with_evidence(
        &self,
        _req: IdleRequest,
        _evidence: StalenessEvidence,
    ) -> Result<HeldLock, AdmissionError> {
        // No cross-instance claims exist to steal.
        Err(AdmissionError::ContentionLost)
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
/// claim root the server derives from its memory dir. Claims are additive
/// unique files; heartbeats advance a sequence in the body; stale claims
/// are stealable only with adapter-built [`StalenessEvidence`].
#[derive(Debug, Clone)]
pub struct ClaimFileAdmission {
    root: PathBuf,
    instance: InstanceId,
    arbiter: SessionArbiter,
    env: AdmissionEnv,
}

impl ClaimFileAdmission {
    /// Build claim-file admission for `root` (e.g. `{memory_dir}/locks`).
    #[must_use]
    pub fn new(root: PathBuf, instance: InstanceId, env: AdmissionEnv) -> Self {
        Self {
            root,
            instance,
            arbiter: SessionArbiter::new(),
            env,
        }
    }

    /// The claim root this backend manages.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl TurnAdmission for ClaimFileAdmission {
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError> {
        todo!("fill: arbiter → O_EXCL claim create + fsync → lease; aura #421 follow-up")
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn admit_with_evidence(
        &self,
        req: IdleRequest,
        evidence: StalenessEvidence,
    ) -> Result<HeldLock, AdmissionError> {
        todo!(
            "fill: revalidate evidence vs claim file → additive gen+1 claim; \
             aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        todo!("fill: uncached read of newest claim; aura #421 follow-up")
    }
}
