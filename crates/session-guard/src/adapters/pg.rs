//! Postgres-fenced admission (`AURA_SESSION_ADMISSION=pg`): the claims
//! table is the sole cross-instance authority.
//!
//! Claim flow (figure 1 in DESIGN.html): arbiter → S1 via
//! [`ClaimStore`] (capturing the S1 transmission instant as the
//! self-fence's initial anchor) → on `Granted`, the manifest rides the
//! row (no filesystem read), a [`DebrisSweep`] runs at claim time
//! against this backend's bound session root (debris-only, epoch-scoped
//! — I3), then the lease/lock assembly. The heartbeat lease renews over
//! S2; the release action closes over S4 with this claim's fence triple.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::TurnAdmission;
use crate::arbiter::SessionArbiter;
use crate::claim::HolderView;
use crate::config::PgAdmissionEnv;
use crate::identity::{PodId, SessionId};
use crate::lease::{BeatInterval, LeaseTtl, SelfFenceMargin};
use crate::repair::RepairLane;
use crate::state::{AdmissionError, HeldLock, IdleRequest};
use crate::store::{ClaimStore, ParkLatch};

/// Proof that a claim is being assembled by the Postgres backend. The
/// constructor is private to this module, so only [`PgAdmission`] can
/// produce one — which is what pins pg claims to heartbeat leases.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PgLeaseProof(());

/// Multi-instance admission against the Postgres claims authority.
/// Fail-stop (I5): when the store is unreachable, admission returns
/// [`AdmissionError::StoreUnavailable`] and every live lease's next
/// renewal revokes it.
pub struct PgAdmission {
    store: Arc<dyn ClaimStore>,
    pod: PodId,
    root: PathBuf,
    arbiter: SessionArbiter,
    retry_after: Duration,
    beat: BeatInterval,
    ttl: LeaseTtl,
    margin: SelfFenceMargin,
    propagation_window: Duration,
    repair: Arc<dyn RepairLane>,
    proof: PgLeaseProof,
}

impl std::fmt::Debug for PgAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgAdmission")
            .field("pod", &self.pod)
            .field("root", &self.root)
            .field("retry_after", &self.retry_after)
            .field("beat", &self.beat)
            .field("ttl", &self.ttl)
            .field("margin", &self.margin)
            .finish_non_exhaustive()
    }
}

impl PgAdmission {
    pub(crate) fn new(
        store: Arc<dyn ClaimStore>,
        pod: PodId,
        root: PathBuf,
        arbiter: SessionArbiter,
        env: PgAdmissionEnv<'_>,
        repair: Arc<dyn RepairLane>,
    ) -> Self {
        Self {
            store,
            pod,
            root,
            arbiter,
            retry_after: env.retry_after(),
            beat: env.beat(),
            ttl: env.lease_ttl(),
            margin: env.fence_margin(),
            propagation_window: env.propagation_window(),
            repair,
            proof: PgLeaseProof(()),
        }
    }
}

#[async_trait]
impl TurnAdmission for PgAdmission {
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError> {
        todo!(
            "fill: arbiter PendingGuard → mint HolderId → note S1 transmission instant \
             (granted_at) → store.claim(S1 + classify, one internal retry when classify \
             observes a claimable row) → Granted: DebrisSweep::at_claim_time(&manifest, epoch)\
             .sweep(&session_root under self.root) (failure ≠ claim failure), then \
             AcquiredClaim::new_pg(self.proof, granted, root/session, self.store.clone(), \
             self.propagation_window, release-closing-over-S4, renewal, self.repair.clone()) \
             → into_held → confirm(); Busy → AdmissionError::Busy; Parked → AdmissionError::Parked; \
             aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        todo!("fill: store.locate (S6, live leases only) → Remote view; aura #421 follow-up")
    }
}

/// The tokio-postgres [`ClaimStore`]. Connects lazily on first use
/// (fill phase): the factory is sync, and a startup connect would make
/// `build_admission` async for a resource that may never be claimed.
#[derive(Debug)]
pub(crate) struct PgStore {
    url: crate::config::PgUrl,
}

impl PgStore {
    /// A store against the given connection URL. `SCHEMA` is applied at
    /// first connect (greenfield bootstrap — there is no migration).
    pub(crate) fn new(url: crate::config::PgUrl) -> Self {
        Self { url }
    }
}

#[async_trait]
impl ClaimStore for PgStore {
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn claim(
        &self,
        req: &crate::store::ClaimRequest,
    ) -> Result<crate::claim::ClaimOutcome, crate::store::StoreUnavailable> {
        todo!(
            "fill: S1_CLAIM; 0 rows → S1_CLASSIFY → Busy/Parked (one S1 retry when claimable); aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn heartbeat(
        &self,
        claim: &crate::store::ClaimRef,
        ttl: LeaseTtl,
    ) -> Result<crate::store::HeartbeatOutcome, crate::store::StoreUnavailable> {
        todo!("fill: S2_HEARTBEAT; 1 row → Renewed, 0 rows → Lost; aura #421 follow-up")
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn commit(
        &self,
        claim: &crate::store::ClaimRef,
        latch: ParkLatch,
        op: crate::identity::OpId,
        manifest: &crate::manifest::Manifest,
    ) -> Result<crate::store::CommitOutcome, crate::store::StoreUnavailable> {
        todo!(
            "fill: S3_COMMIT (manifest as jsonb; $6 = claim.turn when Parked else NULL); 0 rows → LostFence; aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn release(
        &self,
        claim: &crate::store::ClaimRef,
    ) -> Result<crate::store::ReleaseOutcome, crate::store::StoreUnavailable> {
        todo!("fill: S4_RELEASE; 0 rows → Superseded (not an error); aura #421 follow-up")
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn release_pod(&self, pod: &PodId) -> Result<u64, crate::store::StoreUnavailable> {
        todo!("fill: S4_RELEASE_POD, rows-affected; aura #421 follow-up")
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn reconcile_commit(
        &self,
        session: &SessionId,
        claim: &crate::store::ClaimRef,
        op: crate::identity::OpId,
    ) -> Result<crate::store::CommitDisposition, crate::store::StoreUnavailable> {
        todo!(
            "fill: S5_RECONCILE; compare op AND fence triple → Applied / NotApplied / SupersededUnknown; aura #421 follow-up"
        )
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    async fn locate(
        &self,
        session: &SessionId,
    ) -> Result<Option<HolderView>, crate::store::StoreUnavailable> {
        todo!("fill: S6_LOCATE; HolderView::remote(pod, deadline); aura #421 follow-up")
    }
}
