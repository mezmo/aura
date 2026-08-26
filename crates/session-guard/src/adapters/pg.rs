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
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tokio_postgres::types::ToSql;

use crate::TurnAdmission;
use crate::arbiter::SessionArbiter;
use crate::claim::{BusyClaim, ClaimOutcome, GrantedClaim, HolderView, ParkedClaim};
use crate::config::PgAdmissionEnv;
use crate::epoch::{Epoch, session_dir};
use crate::gc::DebrisSweep;
use crate::identity::{HolderId, OpId, PodId, SessionId, TurnId};
use crate::lease::{BeatInterval, LeaseDeadline, LeaseTtl, SelfFenceMargin};
use crate::manifest::Manifest;
use crate::repair::RepairLane;
use crate::state::{
    AcquiredClaim, AdmissionError, HeartbeatRenewal, HeldLock, IdleRequest, ReleaseAction,
    RenewalWrite,
};
use crate::store::{
    ClaimRef, ClaimRequest, ClaimStore, CommitDisposition, CommitOutcome, HeartbeatOutcome,
    ParkLatch, ReleaseOutcome, S1_CLAIM, S1_CLASSIFY, S2_HEARTBEAT, S3_COMMIT, S4_RELEASE,
    S4_RELEASE_POD, S5_RECONCILE, S6_LOCATE, SCHEMA, StoreUnavailable,
};

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

    /// One S4 release bound to this claim's fence triple. Release is
    /// idempotent, so both [`ReleaseOutcome::Released`] and
    /// [`ReleaseOutcome::Superseded`] are success; a store failure surfaces
    /// as a [`crate::state::ReleaseError`]. The returned action owns a
    /// cloned store handle and the claim ref, so it is `Send + 'static`.
    fn release_action(&self, claim: ClaimRef) -> ReleaseAction {
        let store = self.store.clone();
        Box::new(move || {
            Box::pin(async move {
                store.release(&claim).await?;
                Ok(())
            })
        })
    }

    /// One S2 renewal bound to this claim's fence triple. Fails closed (I5):
    /// a lost lease and an unreachable store both revoke, mapped to an
    /// `io::Error`. The `FnMut` owns a cloned store handle, the ttl, and the
    /// claim ref, re-cloning them into each beat's future so every future is
    /// `Send + 'static`.
    fn renewal_write(&self, claim: ClaimRef) -> Box<RenewalWrite> {
        let store = self.store.clone();
        let ttl = self.ttl;
        Box::new(move || {
            let store = store.clone();
            let claim = claim.clone();
            Box::pin(async move {
                match store.heartbeat(&claim, ttl).await {
                    Ok(HeartbeatOutcome::Renewed { .. }) => Ok(()),
                    Ok(HeartbeatOutcome::Lost) => {
                        Err(std::io::Error::other("session claim lease lost"))
                    }
                    Err(unavailable) => Err(std::io::Error::other(unavailable)),
                }
            })
        })
    }
}

#[async_trait]
impl TurnAdmission for PgAdmission {
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError> {
        // The in-process arbiter is the first serialization layer: a lost
        // local race names this pod as the busy holder and never reaches S1.
        let Some(pending) = self.arbiter.try_acquire(&req.session) else {
            return Err(AdmissionError::Busy {
                holder: self.pod.clone(),
                retry_after: self.retry_after,
            });
        };
        // `pending` frees the arbiter slot on drop; it is consumed by
        // `confirm()` only on the granted path, so every error return below
        // (including the `?` on `claim`) drops it and releases the slot.
        let claim_request = ClaimRequest {
            session: req.session.clone(),
            turn: req.turn,
            holder: HolderId::mint(),
            pod: self.pod.clone(),
            ttl: self.ttl,
        };
        match self.store.claim(&claim_request).await? {
            ClaimOutcome::Granted(granted) => {
                let session_root = session_dir(&self.root, &granted.session);
                // Debris GC at claim time against this claim's bound root; a
                // sweep failure is not a claim failure (I3), so its outcome
                // is discarded rather than propagated.
                let _ = DebrisSweep::at_claim_time(&granted.manifest, granted.epoch)
                    .sweep(&session_root)
                    .await;
                let claim_ref = ClaimRef {
                    session: granted.session.clone(),
                    turn: granted.turn,
                    epoch: granted.epoch,
                    holder: granted.holder,
                };
                let renewal = HeartbeatRenewal {
                    beat: self.beat,
                    ttl: self.ttl,
                    margin: self.margin,
                    write: self.renewal_write(claim_ref.clone()),
                };
                let acquired = AcquiredClaim::new_pg(
                    self.proof,
                    granted,
                    session_root,
                    self.store.clone(),
                    self.repair.clone(),
                    self.propagation_window,
                    self.release_action(claim_ref),
                    renewal,
                );
                Ok(acquired.into_held(pending.confirm()))
            }
            // The store fills a zero placeholder for a live-lease refusal, so
            // the honest retry hint is this backend's configured one, never
            // `busy.retry_after` (codex M7).
            ClaimOutcome::Busy(busy) => Err(AdmissionError::Busy {
                holder: busy.holder.pod().clone(),
                retry_after: self.retry_after,
            }),
            ClaimOutcome::Parked(parked) => Err(AdmissionError::Parked { turn: parked.turn }),
        }
    }

    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>> {
        // S6 reports only a live lease; an unreachable store fails loud (I5)
        // rather than masquerading as "no holder".
        self.store
            .locate(session)
            .await
            .map_err(std::io::Error::other)
    }
}

/// The tokio-postgres [`ClaimStore`]. Connects lazily on first use
/// (fill phase): the factory is sync, and a startup connect would make
/// `build_admission` async for a resource that may never be claimed.
#[derive(Debug)]
pub(crate) struct PgStore {
    url: crate::config::PgUrl,
    client: OnceCell<tokio_postgres::Client>,
}

impl PgStore {
    /// A store against the given connection URL. `SCHEMA` is applied at
    /// first connect (greenfield bootstrap — there is no migration).
    pub(crate) fn new(url: crate::config::PgUrl) -> Self {
        Self {
            url,
            client: OnceCell::new(),
        }
    }

    /// The connected client, established on first use: connect, spawn the
    /// connection driver, then apply `SCHEMA` once inside the initializer,
    /// so every later call sees a bootstrapped table. A connect, spawn, or
    /// schema failure is a [`StoreUnavailable`] (fail-stop, I5) and leaves
    /// the cell empty, so the next call retries the connect.
    async fn client(&self) -> Result<&tokio_postgres::Client, StoreUnavailable> {
        self.client
            .get_or_try_init(|| async {
                let (client, connection) = tokio_postgres::connect(self.url.as_ref(), NoTls)
                    .await
                    .map_err(StoreUnavailable::msg)?;
                tokio::spawn(async move {
                    // The driver owns the socket and resolves when the
                    // client drops; its result is nothing to act on.
                    let _ = connection.await;
                });
                client
                    .batch_execute(SCHEMA)
                    .await
                    .map_err(StoreUnavailable::msg)?;
                Ok(client)
            })
            .await
    }
}

#[async_trait]
impl ClaimStore for PgStore {
    async fn claim(
        &self,
        req: &crate::store::ClaimRequest,
    ) -> Result<crate::claim::ClaimOutcome, crate::store::StoreUnavailable> {
        let client = self.client().await?;
        if let Some(granted) = run_s1(client, req).await? {
            return Ok(ClaimOutcome::Granted(granted));
        }
        let row = classify(client, req).await?;
        match classify_zero_row(row.expired, row.parked_turn, req.turn) {
            ZeroRowDecision::Parked => Ok(ClaimOutcome::Parked(parked_from(&row))),
            ZeroRowDecision::Busy => Ok(ClaimOutcome::Busy(busy_from(row))),
            ZeroRowDecision::RetryClaim => {
                // A release landed between S1 and the classify read (the
                // row reads claimable again): retry S1 exactly once, then
                // report the second classify rather than a third shape.
                if let Some(granted) = run_s1(client, req).await? {
                    return Ok(ClaimOutcome::Granted(granted));
                }
                let row = classify(client, req).await?;
                match classify_zero_row(row.expired, row.parked_turn, req.turn) {
                    ZeroRowDecision::Parked => Ok(ClaimOutcome::Parked(parked_from(&row))),
                    _ => Ok(ClaimOutcome::Busy(busy_from(row))),
                }
            }
        }
    }

    async fn heartbeat(
        &self,
        claim: &crate::store::ClaimRef,
        ttl: LeaseTtl,
    ) -> Result<crate::store::HeartbeatOutcome, crate::store::StoreUnavailable> {
        let client = self.client().await?;
        let session = claim.session.as_ref();
        let epoch = epoch_to_i64(claim.epoch);
        let holder = as_uuid(claim.holder);
        let ttl_ms = ttl_millis(ttl);
        let params: [&(dyn ToSql + Sync); 4] = [&session, &epoch, &holder, &ttl_ms];
        let row = client
            .query_opt(S2_HEARTBEAT, &params)
            .await
            .map_err(StoreUnavailable::msg)?;
        match row {
            Some(row) => {
                let deadline = LeaseDeadline::new(row.get::<_, SystemTime>("lease_expires_at"));
                Ok(HeartbeatOutcome::Renewed { deadline })
            }
            None => Ok(HeartbeatOutcome::Lost),
        }
    }

    async fn commit(
        &self,
        claim: &crate::store::ClaimRef,
        latch: ParkLatch,
        op: crate::identity::OpId,
        manifest: &crate::manifest::Manifest,
    ) -> Result<crate::store::CommitOutcome, crate::store::StoreUnavailable> {
        let client = self.client().await?;
        let session = claim.session.as_ref();
        let epoch = epoch_to_i64(claim.epoch);
        let holder = as_uuid(claim.holder);
        let manifest_json = serde_json::to_value(manifest).map_err(StoreUnavailable::msg)?;
        let op_uuid = as_uuid(op);
        // The latch value is the claim's own turn, never a free parameter.
        let parked_turn: Option<uuid::Uuid> = match latch {
            ParkLatch::Parked => Some(as_uuid(claim.turn)),
            ParkLatch::NotParked => None,
        };
        let params: [&(dyn ToSql + Sync); 6] = [
            &session,
            &epoch,
            &holder,
            &manifest_json,
            &op_uuid,
            &parked_turn,
        ];
        let affected = client
            .execute(S3_COMMIT, &params)
            .await
            .map_err(StoreUnavailable::msg)?;
        match affected {
            1 => Ok(CommitOutcome::Committed),
            _ => Ok(CommitOutcome::LostFence),
        }
    }

    async fn release(
        &self,
        claim: &crate::store::ClaimRef,
    ) -> Result<crate::store::ReleaseOutcome, crate::store::StoreUnavailable> {
        let client = self.client().await?;
        let session = claim.session.as_ref();
        let epoch = epoch_to_i64(claim.epoch);
        let holder = as_uuid(claim.holder);
        let params: [&(dyn ToSql + Sync); 3] = [&session, &epoch, &holder];
        let affected = client
            .execute(S4_RELEASE, &params)
            .await
            .map_err(StoreUnavailable::msg)?;
        if affected >= 1 {
            Ok(ReleaseOutcome::Released)
        } else {
            Ok(ReleaseOutcome::Superseded)
        }
    }

    async fn release_pod(&self, pod: &PodId) -> Result<u64, crate::store::StoreUnavailable> {
        let client = self.client().await?;
        let pod = pod.as_ref();
        let params: [&(dyn ToSql + Sync); 1] = [&pod];
        client
            .execute(S4_RELEASE_POD, &params)
            .await
            .map_err(StoreUnavailable::msg)
    }

    async fn reconcile_commit(
        &self,
        claim: &crate::store::ClaimRef,
        op: crate::identity::OpId,
    ) -> Result<crate::store::CommitDisposition, crate::store::StoreUnavailable> {
        let client = self.client().await?;
        let session = claim.session.as_ref();
        let params: [&(dyn ToSql + Sync); 1] = [&session];
        let row = client
            .query_opt(S5_RECONCILE, &params)
            .await
            .map_err(StoreUnavailable::msg)?
            .ok_or_else(|| StoreUnavailable::msg("reconcile: no claims row for the session"))?;
        let row_epoch = epoch_from_i64(row.get::<_, i64>("epoch"))?;
        let row_holder = holder_from_uuid(row.get::<_, uuid::Uuid>("holder_id"));
        let row_op = row
            .get::<_, Option<uuid::Uuid>>("last_commit_op")
            .map(op_from_uuid);
        Ok(reconcile_disposition(
            row_epoch,
            row_holder,
            row_op,
            claim.epoch,
            claim.holder,
            op,
        ))
    }

    async fn locate(
        &self,
        session: &SessionId,
    ) -> Result<Option<HolderView>, crate::store::StoreUnavailable> {
        let client = self.client().await?;
        let session = session.as_ref();
        let params: [&(dyn ToSql + Sync); 1] = [&session];
        let row = client
            .query_opt(S6_LOCATE, &params)
            .await
            .map_err(StoreUnavailable::msg)?;
        match row {
            Some(row) => {
                let pod = PodId::parse(row.get::<_, &str>("holder_pod"))
                    .map_err(StoreUnavailable::msg)?;
                let deadline = LeaseDeadline::new(row.get::<_, SystemTime>("lease_expires_at"));
                Ok(Some(HolderView::remote(pod, deadline)))
            }
            None => Ok(None),
        }
    }
}

/// Run S1 once and map a `RETURNING` row to a granted claim. `granted_at`
/// anchors the self-fence at the S1 *transmission* instant
/// ([`GrantedClaim`] doc), so it is captured immediately before the
/// execute; zero updated rows returns `None` for the caller to classify.
async fn run_s1(
    client: &tokio_postgres::Client,
    req: &ClaimRequest,
) -> Result<Option<GrantedClaim>, StoreUnavailable> {
    let session = req.session.as_ref();
    let holder = as_uuid(req.holder);
    let pod = req.pod.as_ref();
    let ttl_ms = ttl_millis(req.ttl);
    let turn = as_uuid(req.turn);
    let params: [&(dyn ToSql + Sync); 5] = [&session, &holder, &pod, &ttl_ms, &turn];
    let granted_at = Instant::now();
    let row = client
        .query_opt(S1_CLAIM, &params)
        .await
        .map_err(StoreUnavailable::msg)?;
    match row {
        Some(row) => Ok(Some(granted_from_row(&row, req, granted_at)?)),
        None => Ok(None),
    }
}

/// Assemble the granted claim from S1's `RETURNING` row: the epoch fails
/// loud on 0, the deadline is the server-computed `lease_expires_at`, and
/// the manifest is decoded from the row's jsonb. Session and pod are
/// cloned from the borrowed request; turn and holder are the attempt's own.
fn granted_from_row(
    row: &tokio_postgres::Row,
    req: &ClaimRequest,
    granted_at: Instant,
) -> Result<GrantedClaim, StoreUnavailable> {
    let epoch = epoch_from_i64(row.get::<_, i64>("epoch"))?;
    let lease_expires_at = LeaseDeadline::new(row.get::<_, SystemTime>("lease_expires_at"));
    let manifest = serde_json::from_value::<Manifest>(row.get::<_, serde_json::Value>("manifest"))
        .map_err(StoreUnavailable::msg)?;
    Ok(GrantedClaim {
        session: req.session.clone(),
        turn: req.turn,
        epoch,
        holder: req.holder,
        pod: req.pod.clone(),
        lease_expires_at,
        manifest,
        granted_at,
    })
}

/// The parsed S1 classify read: the current holder, the park latch value,
/// and the server-computed expiry flag.
struct ClassifyRow {
    holder: HolderView,
    parked_turn: Option<TurnId>,
    expired: bool,
}

/// Read and parse the classify row for a session whose S1 matched zero
/// rows. The row must exist (the table never deletes), so its absence is
/// a broken invariant and fails loud.
async fn classify(
    client: &tokio_postgres::Client,
    req: &ClaimRequest,
) -> Result<ClassifyRow, StoreUnavailable> {
    let session = req.session.as_ref();
    let params: [&(dyn ToSql + Sync); 1] = [&session];
    let row = client
        .query_opt(S1_CLASSIFY, &params)
        .await
        .map_err(StoreUnavailable::msg)?
        .ok_or_else(|| {
            StoreUnavailable::msg("S1 matched zero rows but the classify read found no row")
        })?;
    let pod = PodId::parse(row.get::<_, &str>("holder_pod")).map_err(StoreUnavailable::msg)?;
    let deadline = LeaseDeadline::new(row.get::<_, SystemTime>("lease_expires_at"));
    let parked_turn = row
        .get::<_, Option<uuid::Uuid>>("parked_turn")
        .map(turn_from_uuid);
    Ok(ClassifyRow {
        holder: HolderView::remote(pod, deadline),
        parked_turn,
        expired: row.get::<_, bool>("expired"),
    })
}

/// The three shapes a zero-row S1 resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZeroRowDecision {
    /// Retry the single S1.
    RetryClaim,
    /// A live lease holds the session.
    Busy,
    /// The session is parked on a different turn.
    Parked,
}

/// Classify a zero-row S1 from the server-side classify read. A row
/// parked on another turn is `Parked` regardless of expiry (S1's `WHERE`
/// excluded it either way); an expired row not parked on another turn
/// read as claimable between S1 and the read, so retry; anything else is
/// a live lease.
fn classify_zero_row(
    expired: bool,
    parked_turn: Option<TurnId>,
    our_turn: TurnId,
) -> ZeroRowDecision {
    match parked_turn {
        Some(turn) if turn != our_turn => ZeroRowDecision::Parked,
        _ if expired => ZeroRowDecision::RetryClaim,
        _ => ZeroRowDecision::Busy,
    }
}

/// A `Parked` decision names the turn the session is parked on (the row's
/// parked turn, present by the [`classify_zero_row`] `Parked` arm).
fn parked_from(row: &ClassifyRow) -> ParkedClaim {
    ParkedClaim {
        turn: row
            .parked_turn
            .expect("classify_zero_row yields Parked only when parked_turn is present"),
    }
}

/// A `Busy` decision carries the holder view; the retry hint is
/// `Duration::ZERO` here and the pg adapter attaches the configured hint
/// when it maps `ClaimOutcome::Busy` to `AdmissionError::Busy` (a zero-row
/// S1 carries no deadline of its own — codex M7).
fn busy_from(row: ClassifyRow) -> BusyClaim {
    BusyClaim {
        holder: row.holder,
        retry_after: Duration::ZERO,
    }
}

/// Compare a reconcile read-back against the claim's fence and op: a
/// different epoch or holder means we were superseded (the op slot may
/// have been overwritten, so the outcome is unknowable); a matching fence
/// with our op id proves the commit landed, and any other op (including
/// NULL) proves it did not.
fn reconcile_disposition(
    row_epoch: Epoch,
    row_holder: HolderId,
    row_op: Option<OpId>,
    fence_epoch: Epoch,
    fence_holder: HolderId,
    op: OpId,
) -> CommitDisposition {
    if row_epoch != fence_epoch || row_holder != fence_holder {
        CommitDisposition::SupersededUnknown
    } else if row_op == Some(op) {
        CommitDisposition::Applied
    } else {
        CommitDisposition::NotApplied
    }
}

/// The identity newtypes wrap a private `uuid::Uuid`; the driver binds a
/// raw `uuid`. The conversion round-trips through the canonical string
/// (`Display` and `Uuid::parse_str` are exact inverses) — the only path
/// across the newtype's opacity from this module.
fn as_uuid(id: impl std::fmt::Display) -> uuid::Uuid {
    uuid::Uuid::parse_str(&id.to_string()).expect("identity Display is always a canonical uuid")
}

/// Reconstruct a [`HolderId`] from a row's `uuid` (canonical, so parsing
/// never fails).
fn holder_from_uuid(raw: uuid::Uuid) -> HolderId {
    HolderId::parse(&raw.to_string()).expect("row uuid always parses as a HolderId")
}

/// Reconstruct an [`OpId`] from a row's `uuid` (canonical, so parsing
/// never fails).
fn op_from_uuid(raw: uuid::Uuid) -> OpId {
    OpId::parse(&raw.to_string()).expect("row uuid always parses as an OpId")
}

/// Reconstruct a [`TurnId`] from a row's `uuid` (canonical, so parsing
/// never fails).
fn turn_from_uuid(raw: uuid::Uuid) -> TurnId {
    TurnId::parse(&raw.to_string()).expect("row uuid always parses as a TurnId")
}

/// The lease ttl as float8 milliseconds, matching S1/S2's
/// `$n * interval '1 millisecond'`.
fn ttl_millis(ttl: LeaseTtl) -> f64 {
    ttl.get().as_secs_f64() * 1_000.0
}

/// The epoch as the column's signed `bigint`.
fn epoch_to_i64(epoch: Epoch) -> i64 {
    epoch.as_u64() as i64
}

/// The epoch from a `bigint` column, failing loud on a below-initial
/// (0 or negative) value rather than smuggling it into fence math.
fn epoch_from_i64(raw: i64) -> Result<Epoch, StoreUnavailable> {
    Epoch::from_raw(raw as u64)
        .ok_or_else(|| StoreUnavailable::msg(format!("claims row carries invalid epoch {raw}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(byte: u8) -> TurnId {
        TurnId::parse(&format!("00000000-0000-0000-0000-0000000000{byte:02x}"))
            .expect("valid turn uuid")
    }

    fn holder(byte: u8) -> HolderId {
        HolderId::parse(&format!("00000000-0000-0000-0000-0000000000{byte:02x}"))
            .expect("valid holder uuid")
    }

    fn op(byte: u8) -> OpId {
        OpId::parse(&format!("00000000-0000-0000-0000-0000000000{byte:02x}"))
            .expect("valid op uuid")
    }

    fn epoch(raw: u64) -> Epoch {
        Epoch::from_raw(raw).expect("non-zero epoch")
    }

    #[test]
    fn zero_row_parked_on_other_turn_is_parked() {
        assert_eq!(
            classify_zero_row(false, Some(turn(2)), turn(1)),
            ZeroRowDecision::Parked
        );
        // Parked wins even once the lease has expired.
        assert_eq!(
            classify_zero_row(true, Some(turn(2)), turn(1)),
            ZeroRowDecision::Parked
        );
    }

    #[test]
    fn zero_row_expired_and_claimable_retries() {
        assert_eq!(
            classify_zero_row(true, None, turn(1)),
            ZeroRowDecision::RetryClaim
        );
        // Parked on our own turn and expired is a reify that came free.
        assert_eq!(
            classify_zero_row(true, Some(turn(1)), turn(1)),
            ZeroRowDecision::RetryClaim
        );
    }

    #[test]
    fn zero_row_live_lease_is_busy() {
        assert_eq!(
            classify_zero_row(false, None, turn(1)),
            ZeroRowDecision::Busy
        );
        assert_eq!(
            classify_zero_row(false, Some(turn(1)), turn(1)),
            ZeroRowDecision::Busy
        );
    }

    #[test]
    fn reconcile_matching_fence_and_op_is_applied() {
        assert_eq!(
            reconcile_disposition(
                epoch(5),
                holder(1),
                Some(op(0xaa)),
                epoch(5),
                holder(1),
                op(0xaa)
            ),
            CommitDisposition::Applied
        );
    }

    #[test]
    fn reconcile_matching_fence_other_op_is_not_applied() {
        assert_eq!(
            reconcile_disposition(
                epoch(5),
                holder(1),
                Some(op(0xbb)),
                epoch(5),
                holder(1),
                op(0xaa)
            ),
            CommitDisposition::NotApplied
        );
        // A NULL op slot is likewise a definite non-landing.
        assert_eq!(
            reconcile_disposition(epoch(5), holder(1), None, epoch(5), holder(1), op(0xaa)),
            CommitDisposition::NotApplied
        );
    }

    #[test]
    fn reconcile_different_fence_is_superseded_unknown() {
        assert_eq!(
            reconcile_disposition(
                epoch(6),
                holder(1),
                Some(op(0xaa)),
                epoch(5),
                holder(1),
                op(0xaa)
            ),
            CommitDisposition::SupersededUnknown
        );
        assert_eq!(
            reconcile_disposition(
                epoch(5),
                holder(2),
                Some(op(0xaa)),
                epoch(5),
                holder(1),
                op(0xaa)
            ),
            CommitDisposition::SupersededUnknown
        );
    }

    // ---- U4: the PgAdmission claim flow, DB-free against a scripted store ----

    use crate::store::scripted::ScriptedStore;

    /// A distinct, non-zero retry hint, so a forwarded `busy.retry_after`
    /// (the store's zero placeholder) is a visibly wrong value.
    const TEST_RETRY_AFTER: Duration = Duration::from_millis(1_234);

    /// A repair lane that is never exercised on the admission path (repair
    /// rides the read path only); both cures are inert here.
    struct InertRepair;

    #[async_trait]
    impl RepairLane for InertRepair {
        async fn refresh_dir(
            &self,
            _dir: &std::path::Path,
        ) -> Result<(), crate::repair::RepairError> {
            Ok(())
        }

        async fn force_cure(
            &self,
            _dir: &std::path::Path,
        ) -> Result<(), crate::repair::RepairError> {
            Ok(())
        }
    }

    fn session() -> SessionId {
        SessionId::parse("s1").expect("valid session id")
    }

    fn pod() -> PodId {
        PodId::parse("pod-0").expect("valid pod id")
    }

    fn remote_pod() -> PodId {
        PodId::parse("remote-pod").expect("valid pod id")
    }

    fn idle_request() -> IdleRequest {
        IdleRequest {
            session: session(),
            turn: TurnId::new(),
        }
    }

    fn granted_claim() -> GrantedClaim {
        GrantedClaim {
            session: session(),
            turn: TurnId::new(),
            epoch: Epoch::initial(),
            holder: HolderId::mint(),
            pod: pod(),
            lease_expires_at: LeaseDeadline::new(SystemTime::now() + Duration::from_secs(15)),
            manifest: Manifest::empty(),
            granted_at: Instant::now(),
        }
    }

    /// A [`PgAdmission`] over a scripted store, assembled by struct literal:
    /// the `PgAdmissionEnv`/`AdmissionEnv` constructor path is confined to
    /// the `config` module tree and is unreachable here, but this test
    /// module is a descendant of `adapters::pg`, so it may name the private
    /// fields and the [`PgLeaseProof`] token directly. The beat is far
    /// larger than any test's lifetime, so the heartbeat actor never fires a
    /// scripted renewal before the [`HeldLock`] drops.
    fn admission(store: Arc<dyn ClaimStore>, arbiter: SessionArbiter) -> PgAdmission {
        PgAdmission {
            store,
            pod: pod(),
            root: PathBuf::from("/nonexistent-sg-test-root"),
            arbiter,
            retry_after: TEST_RETRY_AFTER,
            beat: BeatInterval::new(Duration::from_secs(3_600)).expect("non-zero beat"),
            ttl: LeaseTtl::new(Duration::from_secs(15)).expect("non-zero ttl"),
            margin: SelfFenceMargin::new(Duration::from_millis(250)).expect("non-zero margin"),
            propagation_window: Duration::from_secs(30),
            repair: Arc::new(InertRepair),
            proof: PgLeaseProof(()),
        }
    }

    #[tokio::test]
    async fn pg_admit_granted_returns_held_lock_and_records_hold() {
        let store = Arc::new(ScriptedStore::default());
        store.script_claim(Ok(ClaimOutcome::Granted(granted_claim())));
        let arbiter = SessionArbiter::new();
        let admission = admission(store, arbiter.clone());

        let held = admission
            .admit(idle_request())
            .await
            .expect("a scripted grant yields a held lock");

        assert_eq!(held.holder_view().pod(), &pod());
        assert!(
            arbiter.holds(&session()),
            "a granted admission promotes the arbiter slot to Held"
        );
    }

    #[tokio::test]
    async fn pg_admit_busy_reports_configured_retry_after_not_store_zero() {
        let store = Arc::new(ScriptedStore::default());
        store.script_claim(Ok(ClaimOutcome::Busy(BusyClaim {
            holder: HolderView::remote(
                remote_pod(),
                LeaseDeadline::new(SystemTime::now() + Duration::from_secs(5)),
            ),
            // The store returns a zero placeholder; the adapter must replace
            // it with the configured hint.
            retry_after: Duration::ZERO,
        })));
        let admission = admission(store, SessionArbiter::new());

        let err = admission
            .admit(idle_request())
            .await
            .expect_err("a live lease refuses admission");

        match err {
            AdmissionError::Busy {
                holder,
                retry_after,
            } => {
                assert_eq!(holder, remote_pod(), "the refusal names the store's holder");
                assert_eq!(
                    retry_after, TEST_RETRY_AFTER,
                    "the retry hint is the configured one, never the store's zero"
                );
                assert_ne!(retry_after, Duration::ZERO);
            }
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pg_admit_parked_reports_parked_turn() {
        let parked_turn = TurnId::new();
        let store = Arc::new(ScriptedStore::default());
        store.script_claim(Ok(ClaimOutcome::Parked(ParkedClaim { turn: parked_turn })));
        let admission = admission(store, SessionArbiter::new());

        let err = admission
            .admit(idle_request())
            .await
            .expect_err("a parked session refuses admission");

        match err {
            AdmissionError::Parked { turn } => assert_eq!(turn, parked_turn),
            other => panic!("expected Parked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pg_admit_store_unavailable_maps_and_frees_the_slot() {
        let store = Arc::new(ScriptedStore::default());
        store.script_claim(Err(StoreUnavailable::msg("connection refused")));
        let arbiter = SessionArbiter::new();
        let admission = admission(store, arbiter.clone());

        let err = admission
            .admit(idle_request())
            .await
            .expect_err("an unreachable store fails the claim path");
        assert!(matches!(err, AdmissionError::StoreUnavailable(_)));
        // The dropped PendingGuard released the slot, so a fresh attempt can
        // re-enter admission.
        assert!(
            arbiter.try_acquire(&session()).is_some(),
            "a failed admit frees the arbiter slot"
        );
    }

    #[tokio::test]
    async fn pg_admit_second_same_session_is_busy_via_arbiter() {
        let store = Arc::new(ScriptedStore::default());
        // Only one S1 is scripted: the arbiter must refuse the second admit
        // before it can reach the store (a second claim would panic the
        // exhausted queue).
        store.script_claim(Ok(ClaimOutcome::Granted(granted_claim())));
        let admission = admission(store, SessionArbiter::new());

        let _held = admission
            .admit(idle_request())
            .await
            .expect("the first admit grants");
        let err = admission
            .admit(idle_request())
            .await
            .expect_err("a second same-session admit is refused locally");

        match err {
            AdmissionError::Busy {
                holder,
                retry_after,
            } => {
                assert_eq!(holder, pod(), "the local refusal names this pod");
                assert_eq!(retry_after, TEST_RETRY_AFTER);
            }
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pg_locate_holder_maps_remote_and_absent() {
        let store = Arc::new(ScriptedStore::default());
        let remote = HolderView::remote(
            remote_pod(),
            LeaseDeadline::new(SystemTime::now() + Duration::from_secs(5)),
        );
        store.script_locate(Ok(Some(remote.clone())));
        store.script_locate(Ok(None));
        let admission = admission(store, SessionArbiter::new());

        assert_eq!(
            admission
                .locate_holder(&session())
                .await
                .expect("a live lease reports its holder"),
            Some(remote)
        );
        assert_eq!(
            admission
                .locate_holder(&session())
                .await
                .expect("no live lease reports no holder"),
            None
        );
    }

    #[tokio::test]
    async fn pg_locate_holder_store_unavailable_is_io_error() {
        let store = Arc::new(ScriptedStore::default());
        store.script_locate(Err(StoreUnavailable::msg("connection refused")));
        let admission = admission(store, SessionArbiter::new());

        let err = admission
            .locate_holder(&session())
            .await
            .expect_err("an unreachable store fails loud");
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }
}
