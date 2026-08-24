//! Arbiter-only admission (`AURA_SESSION_ADMISSION=off`, the default).

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::TurnAdmission;
use crate::arbiter::SessionArbiter;
use crate::claim::HolderView;
use crate::identity::{PodId, SessionId};
use crate::state::{AdmissionError, HeldLock, IdleRequest};

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
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError> {
        todo!(
            "fill: arbiter PendingGuard → confirm() → \
             AcquiredClaim::new_local(self.proof, session, turn, self.pod.clone(), \
             self.root/session, self.propagation_window, release) → into_held; \
             aura #421 follow-up"
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
