//! Arbiter-only admission (`AURA_SESSION_ADMISSION=off`, the default).

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::TurnAdmission;
use crate::arbiter::SessionArbiter;
use crate::claim::HolderView;
use crate::epoch::session_dir;
use crate::identity::{PodId, SessionId};
use crate::state::{AcquiredClaim, AdmissionError, HeldLock, IdleRequest, ReleaseAction};

/// Proof that a claim is being assembled by the local backend. The
/// constructor is private to this module, so only [`LocalAdmission`] can
/// produce one — which is what pins local claims to static leases.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalLeaseProof(());

/// Same-instance requests still serialize; there is no cross-instance
/// claim. Leases are static (nothing to renew, no self-fence). The base
/// manifest is empty: `off` mode has no cross-claim continuity.
#[derive(Debug)]
pub struct LocalAdmission {
    pod: PodId,
    root: PathBuf,
    arbiter: SessionArbiter,
    retry_after: Duration,
    propagation_window: Duration,
    proof: LocalLeaseProof,
}

impl LocalAdmission {
    pub(crate) fn new(
        pod: PodId,
        root: PathBuf,
        arbiter: SessionArbiter,
        env: crate::config::LocalAdmissionEnv<'_>,
    ) -> Self {
        Self {
            pod,
            root,
            arbiter,
            retry_after: env.retry_after(),
            propagation_window: env.propagation_window(),
            proof: LocalLeaseProof(()),
        }
    }
}

#[async_trait]
impl TurnAdmission for LocalAdmission {
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError> {
        // The arbiter is the whole election in `off` mode, so it runs
        // first and a lost race names this pod as the holder.
        let Some(pending) = self.arbiter.try_acquire(&req.session) else {
            return Err(AdmissionError::Busy {
                holder: self.pod.clone(),
                retry_after: self.retry_after,
            });
        };
        let session_root = session_dir(&self.root, &req.session);
        // No authority to release against; the arbiter slot frees on drop.
        let release: ReleaseAction = Box::new(|| Box::pin(async { Ok(()) }));
        let claim = AcquiredClaim::new_local(
            self.proof,
            req.session,
            req.turn,
            self.pod.clone(),
            session_root,
            self.propagation_window,
            release,
        );
        Ok(claim.into_held(pending.confirm()))
    }

    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        Ok(self
            .arbiter
            .holds(session)
            .then(|| HolderView::here(self.pod.clone())))
    }
}
