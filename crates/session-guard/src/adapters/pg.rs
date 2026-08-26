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

// ---- P29: live-Postgres frames for S1–S6 against a real database ----

/// Live-Postgres integration frames (card P29). Each frame drives a real
/// [`PgStore`] against the database named by `AURA_SESSION_ADMISSION_PG_URL`
/// and exercises one S1–S6 round-trip end to end. The seam under test
/// (`PgStore::new`, the `ClaimStore` methods, `ClaimRequest`/`ClaimRef`) is
/// `pub(crate)`, so these frames live in `src` rather than an external
/// `tests/` file — the U3/U4 `tests/` placeholders are stubs for exactly
/// this reason.
///
/// With the URL unset every frame logs a loud SKIP and returns, so the
/// suite is green whether or not a database is provisioned (a silent green
/// would hide the whole surface). The `session_claims` table persists and
/// frames run concurrently, so every frame allocates a fresh session id
/// (and, where it matters, a fresh pod id) — nothing here assumes an empty
/// table.
#[cfg(test)]
mod livepg {
    use super::*;
    use crate::claim::Locality;
    use crate::config::PgUrl;

    /// A short server lease, crossed deterministically by [`EXPIRY_WAIT_MS`].
    const LEASE_TTL_MS: u64 = 400;
    /// Wait comfortably past [`LEASE_TTL_MS`] so a slow CI box still sees the
    /// server lease clock cross `clock_timestamp()` before the next probe.
    const EXPIRY_WAIT_MS: u64 = 900;
    /// The claim-to-renewal gap in the heartbeat frame: shorter than
    /// [`LEASE_TTL_MS`] so the renewal still finds the lease live, yet long
    /// enough that the renewed deadline is visibly later than the original.
    const PRE_RENEW_GAP_MS: u64 = 80;
    /// A lease long enough that no frame races its own expiry.
    const GENEROUS_TTL_MS: u64 = 10_000;

    /// The connection URL, or a loud skip. Reading the environment is safe
    /// (the crate forbids only `unsafe`, which the 2024 `set_var`/`remove_var`
    /// are — not `var`), which is why these frames need no env mutation.
    fn pg_url_or_skip(test: &str) -> Option<PgUrl> {
        match std::env::var("AURA_SESSION_ADMISSION_PG_URL") {
            Ok(raw) => {
                Some(PgUrl::parse(&raw).expect("AURA_SESSION_ADMISSION_PG_URL is a valid pg url"))
            }
            Err(_) => {
                eprintln!("SKIP livepg {test}: AURA_SESSION_ADMISSION_PG_URL not set");
                None
            }
        }
    }

    /// Applied exactly once per test binary before any frame's own connect.
    static SCHEMA_READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    /// Force one serialized connect (which applies `SCHEMA`) before any
    /// concurrent frame connects, so every later `CREATE TABLE IF NOT EXISTS`
    /// finds the table already present and cannot race the catalog insert.
    async fn ensure_schema(url: &PgUrl) {
        SCHEMA_READY
            .get_or_init(|| async {
                let store = PgStore::new(url.clone());
                let probe = SessionId::parse("p29-schema-probe").expect("valid session id");
                store
                    .locate(&probe)
                    .await
                    .expect("schema bootstrap connect succeeds");
            })
            .await;
    }

    fn ttl_ms(ms: u64) -> LeaseTtl {
        LeaseTtl::new(Duration::from_millis(ms)).expect("non-zero ttl")
    }

    fn unique_session(tag: &str) -> SessionId {
        SessionId::parse(&format!("p29-{tag}-{}", TurnId::new())).expect("valid session id")
    }

    fn unique_pod(tag: &str) -> PodId {
        PodId::parse(&format!("p29-{tag}-{}", TurnId::new())).expect("valid pod id")
    }

    /// A claim request under a fresh throwaway pod (pod identity is
    /// irrelevant to every frame but the `release_pod` one).
    fn claim_request(session: &SessionId, ttl: LeaseTtl) -> ClaimRequest {
        claim_request_on(session, &unique_pod("holder"), ttl)
    }

    /// A claim request under a caller-chosen pod, with a fresh holder minted
    /// per attempt (as the admission path does).
    fn claim_request_on(session: &SessionId, pod: &PodId, ttl: LeaseTtl) -> ClaimRequest {
        ClaimRequest {
            session: session.clone(),
            turn: TurnId::new(),
            holder: HolderId::mint(),
            pod: pod.clone(),
            ttl,
        }
    }

    fn claim_ref(granted: &GrantedClaim) -> ClaimRef {
        ClaimRef {
            session: granted.session.clone(),
            turn: granted.turn,
            epoch: granted.epoch,
            holder: granted.holder,
        }
    }

    fn expect_granted(outcome: ClaimOutcome) -> GrantedClaim {
        match outcome {
            ClaimOutcome::Granted(granted) => granted,
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    /// S1 headline: two holders race one fresh session; the row lock lets
    /// exactly one win (Granted) and the loser re-evaluates the WHERE against
    /// the winner's live lease and matches zero rows (Busy) — READ COMMITTED
    /// serialization (I7).
    #[tokio::test]
    async fn livepg_upsert_race_one_winner() {
        let Some(url) = pg_url_or_skip("livepg_upsert_race_one_winner") else {
            return;
        };
        ensure_schema(&url).await;

        let session = unique_session("race");
        // Two independent connections so the two S1 upserts genuinely race at
        // the row lock, not on one pipelined client.
        let store_a = PgStore::new(url.clone());
        let store_b = PgStore::new(url.clone());
        // Force both connects up front so `join!` races only the S1 statements.
        store_a.locate(&session).await.expect("store A connects");
        store_b.locate(&session).await.expect("store B connects");

        let req_a = claim_request(&session, ttl_ms(GENEROUS_TTL_MS));
        let req_b = claim_request(&session, ttl_ms(GENEROUS_TTL_MS));
        let (out_a, out_b) = tokio::join!(store_a.claim(&req_a), store_b.claim(&req_b));
        let out_a = out_a.expect("store A claim reaches the db");
        let out_b = out_b.expect("store B claim reaches the db");

        let outcomes = [out_a, out_b];
        let granted = outcomes
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::Granted(_)))
            .count();
        let busy = outcomes
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::Busy(_)))
            .count();
        assert_eq!(
            (granted, busy),
            (1, 1),
            "exactly one racer wins the row lock (Granted); the loser re-evaluates \
             the WHERE against the winner's live lease and is Busy; got {:?} / {:?}",
            outcomes[0],
            outcomes[1],
        );
    }

    /// S1 steal: a short lease lapses server-side, and the next claim by a
    /// new holder steals with an incremented epoch, leaving the first holder
    /// stale.
    #[tokio::test]
    async fn livepg_steal_at_expiry() {
        let Some(url) = pg_url_or_skip("livepg_steal_at_expiry") else {
            return;
        };
        ensure_schema(&url).await;
        let store = PgStore::new(url.clone());
        let session = unique_session("steal");

        let first = expect_granted(
            store
                .claim(&claim_request(&session, ttl_ms(LEASE_TTL_MS)))
                .await
                .expect("first claim reaches the db"),
        );
        assert_eq!(
            first.epoch,
            Epoch::initial(),
            "a fresh session starts at the initial epoch"
        );

        tokio::time::sleep(Duration::from_millis(EXPIRY_WAIT_MS)).await;
        let second = expect_granted(
            store
                .claim(&claim_request(&session, ttl_ms(GENEROUS_TTL_MS)))
                .await
                .expect("steal claim reaches the db"),
        );
        assert!(
            second.epoch > first.epoch,
            "the steal bumps the epoch inside the locked update ({} then {})",
            first.epoch,
            second.epoch
        );
        assert_ne!(
            second.holder, first.holder,
            "the steal installs a new holder"
        );

        let stale = store
            .heartbeat(&claim_ref(&first), ttl_ms(GENEROUS_TTL_MS))
            .await
            .expect("heartbeat reaches the db");
        assert!(
            matches!(stale, HeartbeatOutcome::Lost),
            "the stolen-from holder cannot renew (got {stale:?})"
        );
    }

    /// S2: a live lease renews (deadline advances); once the renewed lease
    /// lapses, the next heartbeat is Lost.
    #[tokio::test]
    async fn livepg_heartbeat_renew_and_expire() {
        let Some(url) = pg_url_or_skip("livepg_heartbeat_renew_and_expire") else {
            return;
        };
        ensure_schema(&url).await;
        let store = PgStore::new(url.clone());
        let session = unique_session("beat");

        let granted = expect_granted(
            store
                .claim(&claim_request(&session, ttl_ms(LEASE_TTL_MS)))
                .await
                .expect("claim reaches the db"),
        );
        let claim = claim_ref(&granted);
        let first_deadline = granted.lease_expires_at;

        // Renew while the lease is still live; the new deadline is later.
        tokio::time::sleep(Duration::from_millis(PRE_RENEW_GAP_MS)).await;
        let renewed = store
            .heartbeat(&claim, ttl_ms(LEASE_TTL_MS))
            .await
            .expect("heartbeat reaches the db");
        let new_deadline = match renewed {
            HeartbeatOutcome::Renewed { deadline } => deadline,
            HeartbeatOutcome::Lost => panic!("a live lease renews, got Lost"),
        };
        assert!(
            new_deadline > first_deadline,
            "renewal moves the server deadline forward"
        );

        // Let the renewed lease lapse; the next heartbeat is Lost.
        tokio::time::sleep(Duration::from_millis(EXPIRY_WAIT_MS)).await;
        let lost = store
            .heartbeat(&claim, ttl_ms(LEASE_TTL_MS))
            .await
            .expect("heartbeat reaches the db");
        assert!(
            matches!(lost, HeartbeatOutcome::Lost),
            "an expired lease cannot renew (got {lost:?})"
        );
    }

    /// S3 fence: a stolen-from holder's commit matches zero rows
    /// (LostFence), while the current holder's commit lands (Committed).
    #[tokio::test]
    async fn livepg_commit_fence_rejected_after_steal() {
        let Some(url) = pg_url_or_skip("livepg_commit_fence_rejected_after_steal") else {
            return;
        };
        ensure_schema(&url).await;
        let store = PgStore::new(url.clone());
        let session = unique_session("fence");

        let a = expect_granted(
            store
                .claim(&claim_request(&session, ttl_ms(LEASE_TTL_MS)))
                .await
                .expect("A claim reaches the db"),
        );
        let a_ref = claim_ref(&a);

        tokio::time::sleep(Duration::from_millis(EXPIRY_WAIT_MS)).await;
        let b = expect_granted(
            store
                .claim(&claim_request(&session, ttl_ms(GENEROUS_TTL_MS)))
                .await
                .expect("B steal reaches the db"),
        );
        let b_ref = claim_ref(&b);

        let a_commit = store
            .commit(
                &a_ref,
                ParkLatch::NotParked,
                OpId::mint(),
                &Manifest::empty(),
            )
            .await
            .expect("A commit reaches the db");
        assert!(
            matches!(a_commit, CommitOutcome::LostFence),
            "a stolen-from holder cannot commit (got {a_commit:?})"
        );

        let b_commit = store
            .commit(
                &b_ref,
                ParkLatch::NotParked,
                OpId::mint(),
                &Manifest::empty(),
            )
            .await
            .expect("B commit reaches the db");
        assert!(
            matches!(b_commit, CommitOutcome::Committed),
            "the current holder commits (got {b_commit:?})"
        );
    }

    /// S5 reconcile: `Applied` (fence + op match a real commit),
    /// `NotApplied` (fence matches, op differs), and `SupersededUnknown`
    /// (a steal changed the fence).
    #[tokio::test]
    async fn livepg_reconcile_three_outcomes() {
        let Some(url) = pg_url_or_skip("livepg_reconcile_three_outcomes") else {
            return;
        };
        ensure_schema(&url).await;
        let store = PgStore::new(url.clone());
        let session = unique_session("recon");

        let a = expect_granted(
            store
                .claim(&claim_request(&session, ttl_ms(GENEROUS_TTL_MS)))
                .await
                .expect("claim reaches the db"),
        );
        let a_ref = claim_ref(&a);
        let op = OpId::mint();

        let committed = store
            .commit(&a_ref, ParkLatch::NotParked, op, &Manifest::empty())
            .await
            .expect("commit reaches the db");
        assert!(
            matches!(committed, CommitOutcome::Committed),
            "the commit lands (got {committed:?})"
        );
        assert_eq!(
            store
                .reconcile_commit(&a_ref, op)
                .await
                .expect("reconcile reaches the db"),
            CommitDisposition::Applied,
            "the same fence and op read back Applied"
        );

        assert_eq!(
            store
                .reconcile_commit(&a_ref, OpId::mint())
                .await
                .expect("reconcile reaches the db"),
            CommitDisposition::NotApplied,
            "the same fence with a different op is a definite non-landing"
        );

        // Release makes the row stealable now; the steal bumps epoch/holder,
        // so the original fence reads back as superseded.
        let released = store.release(&a_ref).await.expect("release reaches the db");
        assert!(
            matches!(released, ReleaseOutcome::Released),
            "the held claim releases (got {released:?})"
        );
        let _b = expect_granted(
            store
                .claim(&claim_request(&session, ttl_ms(GENEROUS_TTL_MS)))
                .await
                .expect("steal reaches the db"),
        );
        assert_eq!(
            store
                .reconcile_commit(&a_ref, op)
                .await
                .expect("reconcile reaches the db"),
            CommitDisposition::SupersededUnknown,
            "a changed epoch/holder makes the original commit's fate unknowable"
        );
    }

    /// S6 locate: a live lease reports its remote holder; an expired lease
    /// reports none.
    #[tokio::test]
    async fn livepg_locate_live_then_expired() {
        let Some(url) = pg_url_or_skip("livepg_locate_live_then_expired") else {
            return;
        };
        ensure_schema(&url).await;
        let store = PgStore::new(url.clone());
        let session = unique_session("locate");

        let req = claim_request(&session, ttl_ms(LEASE_TTL_MS));
        let pod = req.pod.clone();
        let _granted = expect_granted(store.claim(&req).await.expect("claim reaches the db"));

        let holder = store
            .locate(&session)
            .await
            .expect("locate reaches the db")
            .expect("a live lease reports a holder");
        assert_eq!(holder.pod(), &pod, "locate names the holding pod");
        assert_eq!(
            holder.locality(),
            Locality::Remote,
            "a row read is always a remote observation"
        );

        tokio::time::sleep(Duration::from_millis(EXPIRY_WAIT_MS)).await;
        assert!(
            store
                .locate(&session)
                .await
                .expect("locate reaches the db")
                .is_none(),
            "an expired lease locates no holder"
        );
    }

    /// S4: `release` expires a held row (Released, then reads expired); a
    /// stale-fence release is Superseded; `release_pod` expires every live
    /// row a pod holds and returns the count.
    #[tokio::test]
    async fn livepg_release_and_release_pod() {
        let Some(url) = pg_url_or_skip("livepg_release_and_release_pod") else {
            return;
        };
        ensure_schema(&url).await;
        let store = PgStore::new(url.clone());
        let pod = unique_pod("rel");

        // A held claim releases cleanly and then reads expired.
        let session = unique_session("rel-a");
        let a = expect_granted(
            store
                .claim(&claim_request_on(&session, &pod, ttl_ms(GENEROUS_TTL_MS)))
                .await
                .expect("A claim reaches the db"),
        );
        let a_ref = claim_ref(&a);
        let released = store.release(&a_ref).await.expect("release reaches the db");
        assert!(
            matches!(released, ReleaseOutcome::Released),
            "the held claim releases (got {released:?})"
        );
        assert!(
            store
                .locate(&session)
                .await
                .expect("locate reaches the db")
                .is_none(),
            "a released row reads expired"
        );

        // Superseded needs a stale fence: `S4_RELEASE` predicates only on the
        // fence triple (no live check), so a re-release under the SAME fence
        // would still match and read Released. A steal by a new holder bumps
        // the epoch, so the original fence now matches zero rows.
        let _b = expect_granted(
            store
                .claim(&claim_request_on(&session, &pod, ttl_ms(GENEROUS_TTL_MS)))
                .await
                .expect("B steal reaches the db"),
        );
        let superseded = store.release(&a_ref).await.expect("release reaches the db");
        assert!(
            matches!(superseded, ReleaseOutcome::Superseded),
            "a stale fence releases Superseded (got {superseded:?})"
        );

        // release_pod expires every live row under the pod: the stolen
        // session (still held live by B) plus a second live session.
        let session2 = unique_session("rel-b");
        let _c = expect_granted(
            store
                .claim(&claim_request_on(&session2, &pod, ttl_ms(GENEROUS_TTL_MS)))
                .await
                .expect("C claim reaches the db"),
        );
        let expired = store
            .release_pod(&pod)
            .await
            .expect("release_pod reaches the db");
        assert_eq!(
            expired, 2,
            "the pod's two live rows are expired-now and counted"
        );
        assert!(
            store
                .locate(&session)
                .await
                .expect("locate reaches the db")
                .is_none(),
            "the stolen session reads expired after release_pod"
        );
        assert!(
            store
                .locate(&session2)
                .await
                .expect("locate reaches the db")
                .is_none(),
            "the second session reads expired after release_pod"
        );
    }
}
