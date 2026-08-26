//! The consuming state chain: admission → held claim → fenced run →
//! active turn → committing → committed response. Types consume their
//! predecessor by value, so each stage exists only while the turn owns
//! it. Admission output is one sealed [`AcquiredClaim`] — the claim
//! identity, session root, base manifest, backend services, and bound
//! release arrive together, and the lease derives from the same identity
//! inside `into_held`, so identity, lease, and release cannot disagree.
//!
//! The claim identity is the fence triple `(session, epoch, holder)` —
//! every Postgres mutation predicates on it (invariant I2), and every
//! local capability names it.
//!
//! The artifact I/O surface lives here too, and it is the *only* write
//! surface: [`ActiveTurn::write_artifact`] publishes manifest-bound bytes
//! (temp + fsync + rename, one write per path per turn, recording each
//! write into the turn's private delta — I1 and I4 as structure), and
//! [`FencedRun::read_artifact`] owns verify-on-first-read with the
//! three-way miss handling (propagation window → repair lane → fail
//! loud). The raw run-directory path never leaves the crate: the delta
//! the barrier commits is exactly what `write_artifact` recorded, so a
//! fabricated or misattributed manifest is unconstructible. Turn-transient
//! scratchpad I/O has its own typed surface (see `scratchpad.rs`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt;

use crate::arbiter::HeldGuard;
use crate::claim::GrantedClaim;
use crate::epoch::Epoch;
use crate::identity::{HolderId, OpId, PodId, SessionId, TurnId};
use crate::lease::{
    BeatInterval, HeartbeatLease, LeaseLost, LeaseTtl, SelfFenceMargin, WriteCapability,
};
use crate::manifest::{ArtifactPath, DeclareError, Digest, Manifest, ManifestEntry, ReadMiss};
use crate::repair::RepairLane;
use crate::store::{
    ClaimRef, ClaimStore, CommitDisposition, CommitOutcome, ParkLatch, StoreUnavailable,
};

/// The claim's backend services, paired by construction: the pg backend
/// always brings both its store and its repair lane; local brings
/// neither. A store-without-repair (or repair-without-store) claim is
/// unrepresentable.
pub(crate) enum Backend {
    /// Local admission: no authority, no shared mount to cure.
    Local,
    /// Postgres-fenced: the claims store plus the Archil repair lane.
    Pg {
        /// The claims authority.
        store: Arc<dyn ClaimStore>,
        /// The Archil cure seam.
        repair: Arc<dyn RepairLane>,
    },
}

/// The complete output of a successful admission (adapter-side,
/// crate-internal): the claim identity plus the release action bound to
/// that exact claim at construction. The lease is derived from the same
/// identity inside `into_held`. (The closure's captured fence triple
/// cannot be proven by types — closures are opaque; that binding is the
/// adapter's contract, tested in Layer 2 and recorded as residual risk.)
pub(crate) struct AcquiredClaim {
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    pod: PodId,
    session_root: PathBuf,
    manifest: Manifest,
    backend: Backend,
    propagation_window: Duration,
    release: ReleaseAction,
    lease_plan: LeasePlan,
}

/// How the lease for this claim renews. Chosen by the adapter at
/// acquisition (local = static, pg = heartbeat), NOT by the caller at
/// assembly — so the Postgres backend cannot silently take a
/// never-revoking static lease. The heartbeat variant also carries
/// `granted_at`: the self-fence's initial anchor is the claim's S1
/// transmission instant, threaded from the granted claim record — the
/// single source of truth for it (an assembly-time `Instant::now()`
/// would be less conservative and is not accepted anywhere).
pub(crate) enum LeasePlan {
    /// No renewal (local admission): revocation is the only end.
    Static,
    /// Lease-owned heartbeat loop over an adapter S2 write.
    Heartbeat {
        /// Beat cadence (config).
        beat: BeatInterval,
        /// Server-side lease ttl (config).
        ttl: LeaseTtl,
        /// Self-fence margin (config).
        margin: SelfFenceMargin,
        /// The S1 transmission instant (codex M9): the initial anchor,
        /// so the pre-first-beat window is fenced too.
        granted_at: Instant,
        /// One S2 renewal.
        write: Box<RenewalWrite>,
    },
}

/// Renewal inputs for a Postgres heartbeat lease: cadence, ttl, margin,
/// and the S2 write. (The initial anchor is *not* here — it comes from
/// the granted claim record.)
pub(crate) struct HeartbeatRenewal {
    pub(crate) beat: BeatInterval,
    pub(crate) ttl: LeaseTtl,
    pub(crate) margin: SelfFenceMargin,
    pub(crate) write: Box<RenewalWrite>,
}

/// One adapter renewal write (one S2). Renewal failure of any shape —
/// Lost (zero rows) or store-unavailable — maps to an error here, so the
/// lease fails closed either way (I5).
pub(crate) type RenewalWriteFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(), std::io::Error>> + Send>>;
pub(crate) type RenewalWrite = dyn FnMut() -> RenewalWriteFuture + Send;

impl AcquiredClaim {
    /// Assemble a local (static-lease) claim (adapter-side). Epoch and
    /// holder are nominal in local mode (no authority to fence against)
    /// but are still minted, so every downstream identity is real; the
    /// base manifest is empty (no cross-claim continuity in `off` mode).
    /// The `proof` token's constructor is private to `adapters::local`,
    /// so no other backend can build a static-lease claim. The release
    /// action must close over this claim's identity; that binding is the
    /// adapter's contract, tested in Layer 2.
    pub(crate) fn new_local(
        _proof: crate::adapters::local::LocalLeaseProof,
        session: SessionId,
        turn: TurnId,
        pod: PodId,
        session_root: PathBuf,
        propagation_window: Duration,
        release: ReleaseAction,
    ) -> Self {
        Self {
            session,
            turn,
            epoch: Epoch::initial(),
            holder: HolderId::mint(),
            pod,
            session_root,
            manifest: Manifest::empty(),
            backend: Backend::Local,
            propagation_window,
            release,
            lease_plan: LeasePlan::Static,
        }
    }

    /// Assemble a Postgres-fenced (heartbeat-lease) claim (adapter-side)
    /// from the granted claim record. The `proof` token's constructor is
    /// private to `adapters::pg`, so no other backend can build a
    /// heartbeat-lease claim. `write` performs one S2 renewal and must
    /// return only after the statement completes (the same class of
    /// crate-internal contract as the release closure; residual risk,
    /// Layer-2 tested).
    #[expect(clippy::too_many_arguments, reason = "one claim identity bundle")]
    pub(crate) fn new_pg(
        _proof: crate::adapters::pg::PgLeaseProof,
        granted: GrantedClaim,
        session_root: PathBuf,
        store: Arc<dyn ClaimStore>,
        repair: Arc<dyn RepairLane>,
        propagation_window: Duration,
        release: ReleaseAction,
        renewal: HeartbeatRenewal,
    ) -> Self {
        let GrantedClaim {
            session,
            turn,
            epoch,
            holder,
            pod,
            manifest,
            lease_expires_at: _,
            granted_at,
        } = granted;
        let HeartbeatRenewal {
            beat,
            ttl,
            margin,
            write,
        } = renewal;
        Self {
            session,
            turn,
            epoch,
            holder,
            pod,
            session_root,
            manifest,
            backend: Backend::Pg { store, repair },
            propagation_window,
            release,
            lease_plan: LeasePlan::Heartbeat {
                beat,
                ttl,
                margin,
                granted_at,
                write,
            },
        }
    }

    /// Consume the claim into a held lock. The lease is built from this
    /// claim's own identity and its adapter-chosen lease plan inside this
    /// call, so identity, lease, and release cannot disagree, and the
    /// lease kind is never caller-selected.
    pub(crate) fn into_held(self, arbiter: HeldGuard) -> HeldLock {
        let Self {
            session,
            turn,
            epoch,
            holder,
            pod,
            session_root,
            manifest,
            backend,
            propagation_window,
            release,
            lease_plan,
        } = self;
        let source = crate::lease::ClaimLeaseSource {
            session: session.clone(),
            turn,
            epoch,
            holder,
        };
        let lease = match lease_plan {
            LeasePlan::Static => HeartbeatLease::static_from(source),
            LeasePlan::Heartbeat {
                beat,
                ttl,
                margin,
                granted_at,
                write,
            } => HeartbeatLease::with_heartbeat(source, beat, ttl, margin, granted_at, write),
        };
        HeldLock::from_parts(HeldParts {
            session,
            turn,
            epoch,
            holder,
            pod,
            session_root,
            manifest,
            backend,
            propagation_window,
            release,
            arbiter,
            lease,
        })
    }
}

/// Zero-argument release bound to one claim at construction (one S4).
pub(crate) type ReleaseAction = Box<dyn FnOnce() -> ReleaseFuture + Send>;

pub(crate) type ReleaseFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(), ReleaseError>> + Send>>;

/// Why admission failed. Only [`AdmissionError::Busy`] is contention: it
/// maps to HTTP 503 + `Retry-After` and is the sole LB-retryable variant.
/// [`AdmissionError::Parked`] is deliberately not retryable on a hint —
/// the HITL wait is unbounded and routing must surface it honestly.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// A live claim exists for the session.
    #[error("session busy: held by pod {holder}")]
    Busy {
        /// Which pod holds the session, for routing and honest hints.
        holder: PodId,
        /// How long the caller should wait before retrying (configured
        /// fixed hint — a zero-row S1 carries no deadline; codex M7).
        retry_after: Duration,
    },
    /// The session is parked on a different turn, awaiting an external
    /// driver (HITL). Not contention; no retry hint.
    #[error("session parked on turn {turn} (awaiting external resolution)")]
    Parked {
        /// The turn the session is parked on.
        turn: TurnId,
    },
    /// The claim authority is unreachable. Fail-stop (I5): the claim
    /// path fails loudly rather than degrading.
    #[error(transparent)]
    StoreUnavailable(#[from] StoreUnavailable),
    /// The session root is absent or not provisioned (cold-deploy wedge).
    /// Operator-facing, not retryable by the LB. Diagnostic-only payload.
    #[error("session root not provisioned: {0}")]
    NotProvisioned(String),
    /// Claim-path I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A request awaiting admission, parsed and ready. Built by the ingress
/// seam after `SessionId` validation, before any visible status. The
/// acting pod is the backend's own configured identity, not caller
/// input.
#[derive(Debug)]
#[must_use]
pub struct IdleRequest {
    /// The session to claim.
    pub session: SessionId,
    /// The turn this request will run, fixed here so the claim names the
    /// turn even under client retries (and so reify presents the parked
    /// turn id).
    pub turn: TurnId,
}

/// The parts [`HeldLock::from_parts`] assembles (one claim identity
/// bundle; keeps the constructor's arity honest).
pub(crate) struct HeldParts {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) epoch: Epoch,
    pub(crate) holder: HolderId,
    pub(crate) pod: PodId,
    pub(crate) session_root: PathBuf,
    pub(crate) manifest: Manifest,
    pub(crate) backend: Backend,
    pub(crate) propagation_window: Duration,
    pub(crate) release: ReleaseAction,
    pub(crate) arbiter: HeldGuard,
    pub(crate) lease: HeartbeatLease,
}

/// A claim is held for the session. Exists only while the turn owns it;
/// the arbiter guard inside keeps same-instance requests out.
#[must_use]
pub struct HeldLock {
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    pod: PodId,
    /// The session directory this claim is bound to (`{root}/{session}`),
    /// fixed at admission: a claim can never create or sweep beneath a
    /// different session's root.
    session_root: PathBuf,
    /// The committed manifest as granted (the G2 base view).
    manifest: Manifest,
    /// The backend services this claim's commit/release/cures ride.
    backend: Backend,
    /// The read-miss retry budget before repair-lane escalation.
    propagation_window: Duration,
    // Drop order matters: the lease (first) revokes before the arbiter
    // slot (second) frees, so no window exists where another local
    // request acquires the slot while a capability still reads Live.
    lease: HeartbeatLease,
    arbiter: HeldGuard,
    release: ReleaseAction,
}

impl std::fmt::Debug for HeldLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldLock")
            .field("session", &self.session)
            .field("turn", &self.turn)
            .field("epoch", &self.epoch)
            .field("holder", &self.holder)
            .field("pod", &self.pod)
            .finish_non_exhaustive()
    }
}

impl HeldLock {
    /// Assemble from parts (private: only `into_held` calls this,
    /// which is what binds lease to claim by construction).
    fn from_parts(parts: HeldParts) -> Self {
        let HeldParts {
            session,
            turn,
            epoch,
            holder,
            pod,
            session_root,
            manifest,
            backend,
            propagation_window,
            release,
            arbiter,
            lease,
        } = parts;
        Self {
            session,
            turn,
            epoch,
            holder,
            pod,
            session_root,
            manifest,
            backend,
            propagation_window,
            lease,
            arbiter,
            release,
        }
    }

    /// Who holds the session right now (this process, no store read).
    #[must_use]
    pub fn holder_view(&self) -> crate::claim::HolderView {
        crate::claim::HolderView::here(self.pod.clone())
    }

    /// The write capability bound to this claim.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.lease.capability()
    }

    /// Current lease liveness (diagnostics/metrics).
    #[must_use]
    pub fn lease_state(&self) -> crate::lease::LeaseState {
        self.lease.state()
    }

    /// The claim's store handle (crate-internal: the barrier's S3 and
    /// the reconcile read ride it). `None` in local mode.
    pub(crate) fn store(&self) -> Option<&Arc<dyn ClaimStore>> {
        match &self.backend {
            Backend::Local => None,
            Backend::Pg { store, .. } => Some(store),
        }
    }

    /// The claim's repair lane (crate-internal: the read path's
    /// escalation and `create_run`'s EROFS cure). `None` in local mode.
    pub(crate) fn repair_lane(&self) -> Option<&Arc<dyn RepairLane>> {
        match &self.backend {
            Backend::Local => None,
            Backend::Pg { repair, .. } => Some(repair),
        }
    }

    /// The read-miss retry budget (crate-internal: the read path's).
    #[must_use]
    pub(crate) fn propagation_window(&self) -> Duration {
        self.propagation_window
    }

    /// The claim's bound session root (crate-internal: `create_run`,
    /// the sweep, and the write path derive everything under it).
    #[must_use]
    pub(crate) fn session_root(&self) -> &Path {
        &self.session_root
    }

    /// Create and bind this claim's run directory in one step: the
    /// capability is asserted, the epoch dir `e{k}/` derived *from this
    /// claim's epoch under this claim's bound session root* (neither is
    /// caller-selected — the epoch-partition ruling makes the layout
    /// structural), created by this lock, and liveness re-checked after
    /// creation — so the fenced run can never carry a directory
    /// authorized by a different claim or an already-lost lease.
    /// Non-recursive (the session directory must already exist);
    /// `AlreadyExists` is a hard error, not retried.
    ///
    /// Fill note (H4): an `EROFS` from a wedged post-crash delegation
    /// escalates through `self.repair_lane()` (`force_cure` on the
    /// session dir, 518 ms measured) and retries once before failing.
    ///
    /// # Errors
    /// [`CreateRunError`] *returns the lock* in every variant so the
    /// caller can [`abort`](Self::abort) cleanly instead of dropping
    /// into the abandonment window. [`CreateRunError::LostAfterCreate`]
    /// additionally carries the created directory's path so the caller
    /// can remove or quarantine it before aborting — otherwise a fixed
    /// turn id could retry into `AlreadyExists` forever.
    pub async fn create_run(self) -> Result<FencedRun, CreateRunError> {
        let capability = self.lease.capability();
        if let Err(cause) = capability.assert_live() {
            return Err(CreateRunError::NotLive {
                cause: cause.into(),
                lock: self,
            });
        }
        let path = crate::epoch::epoch_dir(&self.session_root, self.epoch);
        if let Err(cause) = create_epoch_dir_with_cure(
            || tokio::fs::create_dir(&path),
            self.repair_lane(),
            self.session_root(),
        )
        .await
        {
            return Err(CreateRunError::Create { cause, lock: self });
        }
        if capability.assert_live().is_err() {
            return Err(CreateRunError::LostAfterCreate {
                lock: self,
                run_dir: path,
            });
        }
        Ok(FencedRun {
            issued: Mutex::new(BTreeSet::new()),
            delta: Mutex::new(Manifest::empty()),
            lock: self,
            run_dir: path,
        })
    }

    /// Early cleanup: the seam failed between admission and the turn
    /// (persistence init, run-dir creation). Revokes the lease and
    /// releases the claim. Both cleanup results are reported; an abort
    /// has no single "failure" to return.
    pub async fn abort(self) -> CleanupOutcome {
        let HeldLock {
            lease,
            release,
            arbiter,
            ..
        } = self;
        // This type retained no run dir, so there is nothing to quarantine;
        // the claim teardown is the whole of the cleanup.
        let release = teardown_claim(lease, release, arbiter).await;
        CleanupOutcome {
            quarantine: Ok(()),
            release,
        }
    }
}

/// Try `create`; on `EROFS`, escalate through `repair` (`force_cure` on
/// `session_root`) and retry `create` once. A missing repair lane (local
/// mode), any non-`EROFS` error, or any error on the retry (including a
/// second `EROFS`) propagates without a further cure or retry — the cure
/// runs at most once per call, regardless of whether it itself succeeds.
async fn create_epoch_dir_with_cure<C, Fut>(
    mut create: C,
    repair: Option<&Arc<dyn RepairLane>>,
    session_root: &Path,
) -> Result<(), std::io::Error>
where
    C: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), std::io::Error>>,
{
    match create().await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::ReadOnlyFilesystem => {
            let Some(repair) = repair else {
                return Err(err);
            };
            // A failed cure is not distinguished from a successful one:
            // the retry runs either way, and its own outcome is what
            // propagates.
            let _ = repair.force_cure(session_root).await;
            create().await
        }
        Err(err) => Err(err),
    }
}

/// `create_run` failed; every variant returns the lock for clean abort.
#[derive(Debug)]
pub enum CreateRunError {
    /// The capability was already lost before anything was created.
    NotLive {
        /// Why liveness failed.
        cause: FenceCause,
        /// The still-held lock.
        lock: HeldLock,
    },
    /// Directory creation failed; nothing was created.
    Create {
        /// The filesystem error.
        cause: std::io::Error,
        /// The still-held lock.
        lock: HeldLock,
    },
    /// The directory was created, then the lease was found lost. The
    /// path is carried so the caller can remove or quarantine the
    /// orphaned directory before aborting — a bare `abort` would leave
    /// it, and a fixed turn id retrying would hit `AlreadyExists`. (The
    /// path is a cleanup payload: it names what to remove, and is the
    /// one place a raw run path crosses the boundary.)
    LostAfterCreate {
        /// The still-held lock.
        lock: HeldLock,
        /// The directory that was created.
        run_dir: PathBuf,
    },
}

/// A verified read of a manifest-referenced artifact: the bytes and the
/// entry they verified against.
#[derive(Debug)]
pub struct VerifiedRead {
    bytes: Vec<u8>,
    entry: ManifestEntry,
}

impl VerifiedRead {
    /// Build one (private: only the read path constructs it, after the
    /// digest check).
    fn new(bytes: Vec<u8>, entry: ManifestEntry) -> Self {
        Self { bytes, entry }
    }

    /// The verified bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The manifest entry the bytes verified against.
    #[must_use]
    pub const fn entry(&self) -> &ManifestEntry {
        &self.entry
    }
}

/// Why a manifest-referenced read failed terminally. The retry/escalate
/// handling (propagation window → repair lane) happens *inside* the read
/// path; what escapes has exhausted both.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The three-way miss classification: `NotFound` past window and
    /// repair, or `Corrupt` on first read.
    #[error(transparent)]
    Miss(#[from] ReadMiss),
    /// Filesystem failure outside the miss classification.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Why an artifact write was rejected before or during publication.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactWriteError {
    /// The path was already written this turn (write-once, I1).
    #[error("artifact path already written this turn: {0}")]
    Duplicate(ArtifactPath),
    /// The path's epoch prefix is not this claim's epoch — writes land
    /// only in the claiming epoch's dir.
    #[error("artifact path {path} is outside this claim's epoch {expected}")]
    WrongEpoch {
        /// The offending path.
        path: ArtifactPath,
        /// This claim's epoch.
        expected: Epoch,
    },
    /// The lease was lost before or during the write.
    #[error(transparent)]
    LeaseLost(#[from] LeaseLost),
    /// Filesystem failure during the write/fsync/rename sequence.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The run directory exists and was created under this claim's authority
/// by [`HeldLock::create_run`]. Isolation only — exclusion is the claim,
/// not the directory. The directory path itself stays crate-internal:
/// all I/O goes through the typed surfaces (`read_artifact`,
/// `write_artifact`, scratchpad ops), so there is no unguarded write
/// path around them.
#[derive(Debug)]
#[must_use]
pub struct FencedRun {
    /// Paths reserved this turn (write-once, I1). A reservation is taken
    /// *synchronously* — before any await or filesystem mutation — so two
    /// concurrent `write_artifact` calls for one path cannot both pass:
    /// the second is rejected before anything is written. A failed write
    /// consumes its reservation (single-artifact retry is unsupported;
    /// the turn aborts). Sync mutex: locked only to insert, never across
    /// an await.
    issued: Mutex<BTreeSet<ArtifactPath>>,
    /// The turn's private manifest delta: every successful
    /// `write_artifact` records itself here. The barrier commits exactly
    /// this — a fabricated or misattributed delta is unconstructible
    /// from outside. Sync mutex: locked only to declare, never across an
    /// await.
    delta: Mutex<Manifest>,
    lock: HeldLock,
    run_dir: PathBuf,
}

impl FencedRun {
    /// The fenced run directory, crate-internal (the write/read/scratch
    /// paths and the sweep derive from it; it never crosses the public
    /// boundary).
    #[must_use]
    pub(crate) fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The committed manifest as granted at claim time (the G2 base
    /// view: every artifact the session has provably committed).
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.lock.manifest
    }

    /// The write capability bound to this claim (persistence checks it
    /// before every write).
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.lock.capability()
    }

    /// Read a manifest-referenced artifact, verifying its digest on
    /// first read. Miss handling is internal: `ENOENT` retries inside
    /// the propagation window, then escalates through the repair lane
    /// (`refresh_dir`, then `force_cure`); a digest mismatch fails loud
    /// immediately (write-once ⇒ mismatch is corruption, not
    /// propagation).
    ///
    /// # Errors
    /// [`ReadError`] after the window and the repair lane are exhausted,
    /// or immediately on corruption.
    pub async fn read_artifact(&self, path: &ArtifactPath) -> Result<VerifiedRead, ReadError> {
        // No committed entry is a terminal miss: the manifest is fixed at
        // claim grant, so retrying or curing it would change nothing.
        let Some(entry) = self.lock.manifest.get(path) else {
            return Err(ReadMiss::NotFound(path.clone()).into());
        };
        let target = self.lock.session_root().join(path);
        let deadline = Instant::now() + self.lock.propagation_window();
        let mut escalation = 0u8;
        loop {
            match tokio::fs::read(&target).await {
                Ok(bytes) => {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&Sha256::digest(&bytes)[..]);
                    let actual = Digest::from_bytes(hash);
                    // Write-once ⇒ a mismatch can only be corruption; fail
                    // loud on first read, never retry it as propagation.
                    if actual == entry.digest {
                        return Ok(VerifiedRead::new(bytes, entry.clone()));
                    }
                    return Err(ReadMiss::Corrupt {
                        path: path.clone(),
                        expected: entry.digest,
                        actual,
                    }
                    .into());
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if Instant::now() >= deadline {
                        return Err(ReadMiss::NotFound(path.clone()).into());
                    }
                    // Escalate the repair lane once per tier between
                    // retries (Pg only); a cure's own failure is benign —
                    // the window keeps retrying, then fails loud. Local
                    // mode has no lane and simply window-retries.
                    if let Some(repair) = self.lock.repair_lane() {
                        match escalation {
                            0 => {
                                let _ = repair.refresh_dir(self.run_dir()).await;
                                escalation = 1;
                            }
                            1 => {
                                let _ = repair.force_cure(self.run_dir()).await;
                                escalation = 2;
                            }
                            _ => {}
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Drain the recorded delta (crate-internal: the barrier consumes it
    /// for the merge + S3).
    #[must_use]
    pub(crate) fn take_delta(&self) -> Manifest {
        std::mem::take(&mut *self.delta.lock().expect("delta mutex poisoned"))
    }

    /// Arm the turn: persistence init (with the capability) happens at
    /// the seam between this call and the first write; a failure there
    /// calls [`abort`](ActiveTurn::abort) on the result.
    pub fn activate(self) -> ActiveTurn {
        ActiveTurn { run: self }
    }
}

/// A live turn. Terminal paths are exactly three:
/// [`complete`](Self::complete) with a [`CommitKind`] (Success or
/// Clarification — both commit), [`park`](Self::park) (HITL-271's
/// commit-then-release with the latch set), or [`abort`](Self::abort)
/// (Failure and mid-turn cancellation — quarantine, no commit).
/// "Success/Clarification commit, Failure aborts" is
/// unrepresentable-to-violate: no failure value can reach the barrier.
#[derive(Debug)]
#[must_use]
pub struct ActiveTurn {
    run: FencedRun,
}

impl ActiveTurn {
    /// The write capability bound to this claim (persistence checks it
    /// before every write).
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.run.capability()
    }

    /// The committed manifest as granted at claim time.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        self.run.manifest()
    }

    /// The turn's run directory (the epoch dir), crate-internal (the
    /// scratchpad I/O surface derives its paths from it).
    #[must_use]
    pub(crate) fn scratch_dir(&self) -> &Path {
        self.run.run_dir()
    }

    /// Read a manifest-referenced artifact (delegates to the fenced
    /// run's verified read path).
    ///
    /// # Errors
    /// [`ReadError`] after the window and the repair lane are exhausted,
    /// or immediately on corruption.
    pub async fn read_artifact(&self, path: &ArtifactPath) -> Result<VerifiedRead, ReadError> {
        self.run.read_artifact(path).await
    }

    /// Write one manifest-bound artifact: the path is *reserved*
    /// synchronously (before any await or filesystem mutation, so
    /// concurrent writers of one path cannot interleave), the capability
    /// is asserted, the epoch prefix must match this claim's, then temp
    /// write → fsync → atomic rename (the vendor-affirmed atomic) →
    /// parent-dir fsync. The digest is computed for the returned entry,
    /// and the write records itself into the turn's private delta — the
    /// barrier commits exactly the recorded set, nothing else. A failed
    /// write consumes the reservation: single-artifact retry is
    /// unsupported, the turn aborts.
    ///
    /// # Errors
    /// [`ArtifactWriteError::Duplicate`] when the path was already
    /// reserved or written this turn; [`ArtifactWriteError::WrongEpoch`]
    /// when the path's prefix is not this claim's epoch;
    /// [`ArtifactWriteError::LeaseLost`] when the capability fails;
    /// [`ArtifactWriteError::Io`] on filesystem failure.
    pub async fn write_artifact(
        &self,
        path: ArtifactPath,
        bytes: &[u8],
    ) -> Result<ManifestEntry, ArtifactWriteError> {
        // Reserve synchronously before any await or filesystem mutation:
        // a second concurrent writer of this path (or an already-committed
        // path) is rejected here, before anything is written. The lock is
        // dropped before the first await, never held across it.
        {
            let mut issued = self.run.issued.lock().expect("issued mutex poisoned");
            if self.run.lock.manifest.get(&path).is_some() || !issued.insert(path.clone()) {
                return Err(ArtifactWriteError::Duplicate(path));
            }
        }
        let capability = self.run.capability();
        capability.assert_live()?;
        let epoch = self.run.lock.epoch;
        if path.epoch() != epoch {
            return Err(ArtifactWriteError::WrongEpoch {
                path,
                expected: epoch,
            });
        }
        // The path is epoch-qualified relative to the session root, and
        // the epoch check above pins it to this claim's epoch dir, so the
        // join lands under `run_dir` (never `run_dir.join` — that would
        // double the epoch prefix).
        let target = self.run.lock.session_root().join(&path);
        let tmp = self
            .run
            .run_dir()
            .join(format!(".sg-artifact-tmp-{}", uuid::Uuid::now_v7()));
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, &target).await?;
        let dir_handle = tokio::fs::File::open(self.run.run_dir()).await?;
        dir_handle.sync_all().await?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&Sha256::digest(bytes)[..]);
        let entry = ManifestEntry {
            digest: Digest::from_bytes(hash),
            turn: capability.turn(),
            epoch,
            bytes: bytes.len() as u64,
        };
        // Record into the turn's private delta: the barrier commits
        // exactly this set. The reservation and epoch check above make a
        // duplicate or foreign-epoch declare unreachable.
        self.run
            .delta
            .lock()
            .expect("delta mutex poisoned")
            .declare(path, entry.clone())
            .expect("reserved own-epoch path declares into the private delta");
        Ok(entry)
    }

    /// End the turn and enter the completion barrier as a commit
    /// (Success or Clarification). After this call the response is not
    /// yet authorized — only the barrier's [`CommittedResponse`] is.
    pub fn complete(self, kind: CommitKind) -> CommittingTurn {
        CommittingTurn {
            run: self.run,
            end: TurnEnd::Commit(kind),
        }
    }

    /// Park the turn (HITL-271's driver): commit the turn's artifacts
    /// and latch the session parked on *this* turn, then release. Reify
    /// re-claims with the same `TurnId`. The latch value is derived by
    /// the barrier, never supplied.
    pub fn park(self) -> CommittingTurn {
        CommittingTurn {
            run: self.run,
            end: TurnEnd::Park,
        }
    }

    /// Abort mid-turn (failure, client disconnect, cancellation):
    /// quarantine the run, stop the lease, release the claim. Distinct
    /// from abandonment: the process is alive to clean up. Nothing
    /// commits — the turn's files stay unreferenced debris for GC. Both
    /// cleanup results are reported; an abort has no single "failure" to
    /// return.
    pub async fn abort(self) -> CleanupOutcome {
        let FencedRun { lock, run_dir, .. } = self.run;
        // Nothing committed, so the run's files are unreferenced: remove
        // them best-effort before tearing the claim down. A removal failure
        // is reported, not fatal — the files stay debris the next sweep
        // collects.
        let quarantine = quarantine_run(&run_dir).await;
        let HeldLock {
            lease,
            release,
            arbiter,
            ..
        } = lock;
        let release = teardown_claim(lease, release, arbiter).await;
        CleanupOutcome {
            quarantine,
            release,
        }
    }
}

/// The committable terminal outcome of a turn (codex B5: the barrier
/// speaks in commit kinds, and Failure is not one — it aborts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    /// The turn produced an answer.
    Success,
    /// The turn ended in a clarification or direct response (commits
    /// like success: the artifact set includes the clarification
    /// record).
    Clarification,
}

impl std::fmt::Display for CommitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitKind::Success => f.write_str("success"),
            CommitKind::Clarification => f.write_str("clarification"),
        }
    }
}

/// How one turn's barrier run leaves the session. One type covers both
/// terminal shapes, so the latch is always derived from the end and
/// never a free parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    /// A committing end: Success or Clarification. The park latch is
    /// written NULL.
    Commit(CommitKind),
    /// A park (HITL-271's driver): commits the turn's artifacts and
    /// latches the session parked on this turn.
    Park,
}

impl TurnEnd {
    /// The latch this end writes at S3 (derived, never free).
    pub(crate) const fn latch(&self) -> ParkLatch {
        match self {
            TurnEnd::Commit(_) => ParkLatch::NotParked,
            TurnEnd::Park => ParkLatch::Parked,
        }
    }
}

impl std::fmt::Display for TurnEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnEnd::Commit(kind) => write!(f, "{kind}"),
            TurnEnd::Park => f.write_str("park"),
        }
    }
}

/// Context handed to the injected payload step. Assembled by the barrier
/// from the turn's own state; fields are private with read accessors, so
/// a context is not constructible or forgeable outside the crate. The
/// barrier mints the [`OpId`] at assembly: one barrier run is one
/// logical commit, and every retry of its S3 reuses this id (B3).
#[derive(Debug, Clone)]
pub struct CommitContext {
    end: TurnEnd,
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    op: OpId,
}

impl CommitContext {
    /// How this barrier run ends the session (commit kind, or park).
    #[must_use]
    pub const fn end(&self) -> TurnEnd {
        self.end
    }

    /// The session.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The turn.
    #[must_use]
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// The claim epoch that authorized the turn.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The acquire attempt that authorized the turn.
    #[must_use]
    pub const fn holder(&self) -> HolderId {
        self.holder
    }

    /// The logical commit id (commit-unknown reconciliation key, B3).
    #[must_use]
    pub const fn op(&self) -> OpId {
        self.op
    }
}

/// How the barrier's cleanup fared when the commit step failed. Both
/// fields carry their real results: no cleanup failure is silently
/// dropped. `#[must_use]`: an ignored abort outcome is a compile-time
/// warning, matching the rest of the state chain.
#[derive(Debug)]
#[must_use]
pub struct CleanupOutcome {
    /// The quarantine result.
    pub quarantine: Result<(), std::io::Error>,
    /// The claim release result.
    pub release: Result<(), ReleaseError>,
}

/// The completion barrier: payload step → merge → S3 → release →
/// authorize, owned by the crate end to end. The injected step produces
/// the response payload only; the manifest delta is *not* its
/// responsibility — the barrier consumes the turn's private recorded
/// delta ([`FencedRun::take_delta`]), merges it into the granted base
/// manifest ([`Manifest::extend_from`], so a colliding or misattributed
/// entry fails the commit), issues S3 through the bound store, stops the
/// lease, releases, and only then authorizes.
#[derive(Debug)]
#[must_use]
pub struct CommittingTurn {
    run: FencedRun,
    end: TurnEnd,
}

impl CommittingTurn {
    /// Run the barrier. The injected step is lazy and context-fed, so no
    /// commit work can precede admission-controlled ordering. Its error
    /// aborts the commit before any S3.
    ///
    /// # Errors
    /// [`BarrierError::CommitFailed`] — the injected step failed;
    /// carries the step's error plus the [`CleanupOutcome`].
    /// [`BarrierError::CommitRejected`] — the crate-side commit failed
    /// *definitely* (delta merge, fence loss, proven non-landing, or
    /// store error); quarantine was attempted.
    /// [`BarrierError::CommitIndeterminate`] — commit-unknown and the
    /// outcome could not be reconciled (supersession *or* a failed
    /// reconcile read): whether S3 landed is unknowable, so the run is
    /// *never* quarantined (its bytes may be manifest-referenced); the
    /// claim is released and the turn reported lost.
    /// [`BarrierError::Release`] — release failed after a durable
    /// commit; the authorized response travels with the error (data is
    /// durable) and the session is flagged wedged.
    pub async fn barrier<T, E, F, Fut>(
        self,
        payload: F,
    ) -> Result<CommittedResponse<T>, BarrierError<T, E>>
    where
        F: FnOnce(CommitContext) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let CommittingTurn { run, end } = self;
        let delta = run.take_delta();
        let FencedRun { lock, run_dir, .. } = run;
        let HeldLock {
            session,
            turn,
            epoch,
            holder,
            manifest: base,
            backend,
            lease,
            arbiter,
            release,
            ..
        } = lock;

        // One barrier run is one logical commit; every retry of its S3
        // reuses this id (B3).
        let op = OpId::mint();
        let ctx = CommitContext {
            end,
            session: session.clone(),
            turn,
            epoch,
            holder,
            op,
        };

        // The injected step runs first and produces only the response
        // payload; its failure aborts before any S3.
        let payload = match payload(ctx).await {
            Ok(payload) => payload,
            Err(error) => {
                let quarantine = quarantine_run(&run_dir).await;
                let release = teardown_claim(lease, release, arbiter).await;
                return Err(BarrierError::CommitFailed {
                    error,
                    cleanup: CleanupOutcome {
                        quarantine,
                        release,
                    },
                });
            }
        };

        // Merge the turn's recorded delta into the granted base; a
        // colliding or misattributed entry fails the commit rather than
        // replacing committed history.
        let mut merged = base;
        if let Err(err) = merged.extend_from(delta) {
            let quarantine = quarantine_run(&run_dir).await;
            let release = teardown_claim(lease, release, arbiter).await;
            return Err(BarrierError::CommitRejected {
                cause: CommitRejection::Declare(err),
                cleanup: CleanupOutcome {
                    quarantine,
                    release,
                },
            });
        }

        // Commit. Local mode has no cross-instance authority, so the merged
        // manifest is the commit and S3/reconcile are skipped. Pg mode
        // issues S3 and, on a lost response, reconciles by read-back.
        if let Backend::Pg { store, .. } = &backend {
            let claim_ref = ClaimRef {
                session: session.clone(),
                turn,
                epoch,
                holder,
            };
            match store.commit(&claim_ref, end.latch(), op, &merged).await {
                Ok(CommitOutcome::Committed) => {}
                Ok(CommitOutcome::LostFence) => {
                    let quarantine = quarantine_run(&run_dir).await;
                    let release = teardown_claim(lease, release, arbiter).await;
                    return Err(BarrierError::CommitRejected {
                        cause: CommitRejection::LostFence,
                        cleanup: CleanupOutcome {
                            quarantine,
                            release,
                        },
                    });
                }
                Err(_unavailable) => {
                    // Commit-unknown: the S3 response was lost. Reconcile by
                    // read-back before deciding — never a blind re-commit.
                    match store.reconcile_commit(&claim_ref, op).await {
                        Ok(CommitDisposition::Applied) => {}
                        Ok(CommitDisposition::NotApplied) => {
                            // Proven never landed: quarantine is safe, nothing
                            // references the run.
                            let quarantine = quarantine_run(&run_dir).await;
                            let release = teardown_claim(lease, release, arbiter).await;
                            return Err(BarrierError::CommitRejected {
                                cause: CommitRejection::NotLanded,
                                cleanup: CleanupOutcome {
                                    quarantine,
                                    release,
                                },
                            });
                        }
                        Ok(CommitDisposition::SupersededUnknown) => {
                            // Whether S3 landed is unknowable, so the run is
                            // never quarantined (its bytes may be
                            // manifest-referenced); release and report lost.
                            let release = teardown_claim(lease, release, arbiter).await;
                            return Err(BarrierError::CommitIndeterminate {
                                cause: IndeterminateCause::SupersededUnknown,
                                release,
                            });
                        }
                        Err(_reconcile_unavailable) => {
                            // No view of the row at all; same indeterminate
                            // rule — never quarantine.
                            let release = teardown_claim(lease, release, arbiter).await;
                            return Err(BarrierError::CommitIndeterminate {
                                cause: IndeterminateCause::ReconcileUnavailable,
                                release,
                            });
                        }
                    }
                }
            }
        }

        // Durable commit: stop the lease, release the claim, then authorize.
        // A release failure after a durable commit travels with the
        // authorized response — the data is durable; delivery is the
        // caller's call.
        let release = teardown_claim(lease, release, arbiter).await;
        let response = CommittedResponse::new(payload, session, turn, epoch, holder);
        match release {
            Ok(()) => Ok(response),
            Err(error) => Err(BarrierError::Release { response, error }),
        }
    }
}

/// Best-effort removal of a turn's run directory. A failure is reported,
/// not fatal: the uncommitted files are unreferenced debris the next sweep
/// collects.
async fn quarantine_run(run_dir: &Path) -> Result<(), std::io::Error> {
    tokio::fs::remove_dir_all(run_dir).await
}

/// Ordered claim teardown: revoke and join the lease first (so every
/// capability fails closed), then run the release action, then free the
/// arbiter slot last — the drop-order invariant that keeps another local
/// request from taking the slot while a capability still reads Live.
async fn teardown_claim(
    lease: HeartbeatLease,
    release: ReleaseAction,
    arbiter: HeldGuard,
) -> Result<(), ReleaseError> {
    lease.stop().await;
    let released = (release)().await;
    drop(arbiter);
    released
}

/// Why the crate-side commit was rejected *definitely* (the injected
/// step succeeded but the commit provably did not become committed
/// state). Indeterminacy is not here — it has its own barrier variant.
#[derive(Debug, thiserror::Error)]
pub enum CommitRejection {
    /// The recorded delta collided with or misattributed committed
    /// state (write-once / provenance enforcement).
    #[error(transparent)]
    Declare(#[from] DeclareError),
    /// The fence triple no longer holds: the claim was stolen before
    /// S3 landed.
    #[error("claim fence lost before commit")]
    LostFence,
    /// Commit-unknown, and the reconcile read proved the commit never
    /// landed (`NotApplied`). Quarantine is safe: nothing references the
    /// run.
    #[error("commit proved never landed")]
    NotLanded,
    /// The claim authority errored *before S3 was dispatched* (nothing
    /// could have landed). A store error after S3 dispatch is NOT here —
    /// it is indeterminate and routes to `CommitIndeterminate`. No
    /// `#[from]` on purpose: every construction site must name the
    /// variant, so the pre-S3-only contract is confronted at each site.
    #[error(transparent)]
    Store(StoreUnavailable),
}

/// Why a commit's outcome is unknowable (the commit may have landed, so
/// the run's bytes may be manifest-referenced — quarantine is forbidden
/// on every variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndeterminateCause {
    /// Commit-unknown, and the reconcile read found the claim
    /// superseded: the single op slot may have been overwritten.
    SupersededUnknown,
    /// Commit-unknown, and the reconcile read itself failed: no view of
    /// the row exists at all.
    ReconcileUnavailable,
}

impl std::fmt::Display for IndeterminateCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndeterminateCause::SupersededUnknown => f.write_str("superseded before reconcile"),
            IndeterminateCause::ReconcileUnavailable => f.write_str("reconcile read unavailable"),
        }
    }
}

/// Why the barrier failed.
#[derive(Debug, thiserror::Error)]
pub enum BarrierError<T, E> {
    /// The injected payload step failed. The run was quarantined (if
    /// possible) and the claim released (if possible); both outcomes are
    /// in `cleanup`, so a failed cleanup is never silently dropped.
    #[error("payload step failed; quarantine_ok={}, released={}", cleanup.quarantine.is_ok(), cleanup.release.is_ok())]
    CommitFailed {
        /// The payload step's own error.
        #[source]
        error: E,
        /// Quarantine and release results.
        cleanup: CleanupOutcome,
    },
    /// The payload step succeeded but the commit was *definitely*
    /// rejected (merge, fence, proven non-landing, store). Quarantine
    /// was attempted; both cleanup outcomes travel.
    #[error("commit rejected ({cause}); quarantine_ok={}, released={}", cleanup.quarantine.is_ok(), cleanup.release.is_ok())]
    CommitRejected {
        /// Why the authority rejected (or disproved) the commit.
        #[source]
        cause: CommitRejection,
        /// Quarantine and release results.
        cleanup: CleanupOutcome,
    },
    /// Commit-indeterminate: the S3 response was lost *and* the outcome
    /// could not be reconciled — either the reconcile read found the
    /// claim superseded, or the reconcile read itself failed. Whether
    /// S3 landed is unknowable, so the run is *never* quarantined (its
    /// bytes may be manifest-referenced); the claim is released and the
    /// turn reported lost.
    #[error("commit indeterminate ({cause}); released={}, run left for manifest-aware GC", release.is_ok())]
    CommitIndeterminate {
        /// Why the outcome is unknowable.
        cause: IndeterminateCause,
        /// The claim release result.
        release: Result<(), ReleaseError>,
    },
    /// The claim release failed after a durable commit. The response is
    /// authorized (carried here; delivery is the caller's call); the
    /// session needs operator attention.
    #[error("release failed after commit; session wedged")]
    Release {
        /// The authorized response.
        response: CommittedResponse<T>,
        /// The release failure.
        #[source]
        error: ReleaseError,
    },
}

impl<T, E> BarrierError<T, E> {
    /// The injected step's error, if this was a step failure.
    #[must_use]
    pub fn commit_error(&self) -> Option<&E> {
        match self {
            Self::CommitFailed { error, .. } => Some(error),
            Self::CommitRejected { .. }
            | Self::CommitIndeterminate { .. }
            | Self::Release { .. } => None,
        }
    }

    /// Take the authorized response out of a release failure, if this
    /// was one. Delivery after a wedge is the caller's decision.
    #[must_use]
    pub fn into_release_response(self) -> Option<CommittedResponse<T>> {
        match self {
            Self::Release { response, .. } => Some(response),
            Self::CommitFailed { .. }
            | Self::CommitRejected { .. }
            | Self::CommitIndeterminate { .. } => None,
        }
    }
}

/// Sole authorization to emit a terminal frame. Carries the committed
/// payload bound to session + turn + epoch + holder. Not `Clone`; the
/// payload is only reachable through authorization-preserving
/// transforms. `#[must_use]`: dropping the envelope unconsumed is a
/// compile-time warning, so the authorization cannot be silently
/// discarded.
#[derive(Debug)]
#[must_use]
pub struct CommittedResponse<T> {
    payload: T,
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
}

impl<T> CommittedResponse<T> {
    /// Build the authorization (private to this module: only the barrier
    /// body constructs it).
    fn new(payload: T, session: SessionId, turn: TurnId, epoch: Epoch, holder: HolderId) -> Self {
        Self {
            payload,
            session,
            turn,
            epoch,
            holder,
        }
    }

    /// Transform the payload while preserving the authorization envelope.
    /// The only public way to reach the payload's value.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> CommittedResponse<U> {
        CommittedResponse {
            payload: f(self.payload),
            session: self.session,
            turn: self.turn,
            epoch: self.epoch,
            holder: self.holder,
        }
    }

    /// Consume the authorization, yielding the payload and its identity
    /// to the terminal emitter that consumes this value.
    #[must_use]
    pub fn into_parts(self) -> (T, SessionId, TurnId, Epoch, HolderId) {
        (
            self.payload,
            self.session,
            self.turn,
            self.epoch,
            self.holder,
        )
    }

    /// Turn identity for logging and metrics.
    #[must_use]
    pub fn turn(&self) -> TurnId {
        self.turn
    }

    /// Session identity for logging and metrics.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

/// Why a claim release failed. Supersession is *not* here: an
/// already-superseded release is a clean idempotent outcome at the store
/// (`ReleaseOutcome::Superseded`), so this enum carries only genuine
/// failures — the barrier can never misreport a steal as a wedge.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The claim authority is unreachable (fail-stop, I5).
    #[error(transparent)]
    StoreUnavailable(#[from] StoreUnavailable),
    /// Filesystem failure while releasing (quarantine path).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Why a fence or write path failed.
#[derive(Debug, thiserror::Error)]
pub enum FenceCause {
    /// The backing claim lease was lost; writes were stopped.
    #[error(transparent)]
    LeaseLost(#[from] LeaseLost),
    /// Filesystem rejection (e.g. EROFS on the shared mount — the H4
    /// wedge class).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::arbiter::SessionArbiter;
    use crate::config::AdmissionEnv;
    use crate::lease::ClaimLeaseSource;
    use crate::repair::build_repair_lane;
    use crate::store::scripted::ScriptedStore;

    /// A wrong `write_artifact` that skips the private-delta record still
    /// passes the public publish frame (bytes land on disk); this pins
    /// the recording itself, so `take_delta` sees exactly the one entry.
    #[test]
    fn write_artifact_records_delta() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = crate::build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let session = SessionId::parse("s1").expect("session id parses");
            tokio::fs::create_dir_all(root.path().join(session.as_ref()))
                .await
                .expect("session root exists for create_run");
            let req = IdleRequest {
                session: session.clone(),
                turn: TurnId::new(),
            };
            let lock = admission.admit(req).await.expect("local admission grants");
            let run = lock.create_run().await.expect("run dir created");
            let active = run.activate();

            let path = ArtifactPath::parse("e1/recorded.txt").expect("path parses");
            let entry = active
                .write_artifact(path.clone(), b"delta")
                .await
                .expect("write succeeds");

            let delta = active.run.take_delta();
            assert_eq!(delta.len(), 1, "exactly one entry recorded in the delta");
            assert_eq!(
                delta.get(&path),
                Some(&entry),
                "the recorded delta entry is the returned entry"
            );
        });
    }

    /// The park latch is derived from the turn end, never supplied. `latch()`
    /// is what the barrier issues at S3, so pin it against drift.
    #[test]
    fn barrier_park_derives_latch() {
        assert_eq!(TurnEnd::Park.latch(), ParkLatch::Parked);
        assert_eq!(
            TurnEnd::Commit(CommitKind::Success).latch(),
            ParkLatch::NotParked
        );
        assert_eq!(
            TurnEnd::Commit(CommitKind::Clarification).latch(),
            ParkLatch::NotParked
        );
    }

    /// Assemble a Postgres-backed [`CommittingTurn`] over a scripted store,
    /// so the barrier's S3-and-reconcile branch is exercised without a
    /// database. The repair lane is never touched on the commit path, so a
    /// CLI lane over a nonexistent binary is an inert placeholder.
    fn pg_committing_turn(store: Arc<ScriptedStore>, run_dir: PathBuf) -> CommittingTurn {
        let session = SessionId::parse("pg-session").expect("session id parses");
        let turn = TurnId::new();
        let epoch = Epoch::initial();
        let holder = HolderId::mint();
        let pod = PodId::parse("pod-0").expect("pod id parses");
        let arbiter = SessionArbiter::new()
            .try_acquire(&session)
            .expect("fresh arbiter admits")
            .confirm();
        let lease = HeartbeatLease::static_from(ClaimLeaseSource {
            session: session.clone(),
            turn,
            epoch,
            holder,
        });
        let session_root = run_dir
            .parent()
            .expect("run dir has a session-root parent")
            .to_path_buf();
        let store: Arc<dyn ClaimStore> = store;
        let release: ReleaseAction = Box::new(|| Box::pin(async { Ok(()) }));
        let lock = HeldLock::from_parts(HeldParts {
            session,
            turn,
            epoch,
            holder,
            pod,
            session_root,
            manifest: Manifest::empty(),
            backend: Backend::Pg {
                store,
                repair: build_repair_lane(),
            },
            propagation_window: Duration::from_millis(0),
            release,
            arbiter,
            lease,
        });
        let run = FencedRun {
            issued: Mutex::new(BTreeSet::new()),
            delta: Mutex::new(Manifest::empty()),
            lock,
            run_dir,
        };
        CommittingTurn {
            run,
            end: TurnEnd::Commit(CommitKind::Success),
        }
    }

    async fn pg_run_dir(root: &std::path::Path) -> PathBuf {
        let run_dir = root.join("pg-session").join("e1");
        tokio::fs::create_dir_all(&run_dir)
            .await
            .expect("run dir exists");
        run_dir
    }

    #[tokio::test]
    async fn pg_barrier_lost_fence_rejects_and_quarantines() {
        let root = tempfile::tempdir().expect("temp root");
        let run_dir = pg_run_dir(root.path()).await;
        let store = Arc::new(ScriptedStore::default());
        store.script_commit(Ok(CommitOutcome::LostFence));
        let committing = pg_committing_turn(store, run_dir.clone());
        let err = committing
            .barrier(|_ctx| async move { Ok::<_, &str>("payload") })
            .await
            .expect_err("a lost fence rejects the commit");
        assert!(matches!(
            err,
            BarrierError::CommitRejected {
                cause: CommitRejection::LostFence,
                ..
            }
        ));
        assert!(!run_dir.exists(), "LostFence quarantines the run dir");
    }

    #[tokio::test]
    async fn pg_barrier_commit_unknown_then_applied_authorizes() {
        let root = tempfile::tempdir().expect("temp root");
        let run_dir = pg_run_dir(root.path()).await;
        let store = Arc::new(ScriptedStore::default());
        store.script_commit(Err(StoreUnavailable::msg("s3 response lost")));
        store.script_reconcile(Ok(CommitDisposition::Applied));
        let committing = pg_committing_turn(store, run_dir.clone());
        let response = committing
            .barrier(|_ctx| async move { Ok::<_, &str>("payload") })
            .await
            .expect("a reconciled-Applied commit-unknown authorizes");
        let (payload, ..) = response.into_parts();
        assert_eq!(payload, "payload");
        assert!(run_dir.exists(), "an applied commit never quarantines");
    }

    #[tokio::test]
    async fn pg_barrier_commit_unknown_superseded_is_indeterminate() {
        let root = tempfile::tempdir().expect("temp root");
        let run_dir = pg_run_dir(root.path()).await;
        let store = Arc::new(ScriptedStore::default());
        store.script_commit(Err(StoreUnavailable::msg("s3 response lost")));
        store.script_reconcile(Ok(CommitDisposition::SupersededUnknown));
        let committing = pg_committing_turn(store, run_dir.clone());
        let err = committing
            .barrier(|_ctx| async move { Ok::<_, &str>("payload") })
            .await
            .expect_err("a superseded-unknown commit is indeterminate");
        assert!(matches!(
            err,
            BarrierError::CommitIndeterminate {
                cause: IndeterminateCause::SupersededUnknown,
                ..
            }
        ));
        assert!(
            run_dir.exists(),
            "an indeterminate commit never quarantines the maybe-referenced run"
        );
    }

    #[tokio::test]
    async fn pg_barrier_reconcile_unavailable_is_indeterminate() {
        let root = tempfile::tempdir().expect("temp root");
        let run_dir = pg_run_dir(root.path()).await;
        let store = Arc::new(ScriptedStore::default());
        store.script_commit(Err(StoreUnavailable::msg("s3 response lost")));
        store.script_reconcile(Err(StoreUnavailable::msg("reconcile read down")));
        let committing = pg_committing_turn(store, run_dir.clone());
        let err = committing
            .barrier(|_ctx| async move { Ok::<_, &str>("payload") })
            .await
            .expect_err("a failed reconcile read is indeterminate");
        assert!(matches!(
            err,
            BarrierError::CommitIndeterminate {
                cause: IndeterminateCause::ReconcileUnavailable,
                ..
            }
        ));
        assert!(
            run_dir.exists(),
            "an indeterminate commit never quarantines the maybe-referenced run"
        );
    }

    use async_trait::async_trait;
    use std::io::{Error as IoError, ErrorKind};

    /// A [`RepairLane`] double that only ever fields `force_cure` calls;
    /// `create_epoch_dir_with_cure` never calls `refresh_dir`, so reaching
    /// it here is a test bug, not a real code path. The call count lives
    /// behind a shared `Arc` so the test retains a handle after the lane
    /// is erased to `Arc<dyn RepairLane>`.
    struct RecordingRepairLane {
        force_cure_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl RepairLane for RecordingRepairLane {
        async fn refresh_dir(&self, _dir: &Path) -> Result<(), crate::repair::RepairError> {
            unreachable!("create_run's cure path escalates only through force_cure")
        }

        async fn force_cure(&self, _dir: &Path) -> Result<(), crate::repair::RepairError> {
            self.force_cure_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Replays a fixed script of outcomes for `create_epoch_dir_with_cure`'s
    /// injected `create` op: `Some(kind)` errors with that kind, `None`
    /// succeeds. Exhausting the script panics — no golden here drives
    /// `create` more times than it scripts.
    struct ScriptedCreate {
        script: std::sync::Mutex<std::collections::VecDeque<Option<ErrorKind>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedCreate {
        fn new(script: impl IntoIterator<Item = Option<ErrorKind>>) -> Self {
            Self {
                script: std::sync::Mutex::new(script.into_iter().collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        async fn call(&self) -> Result<(), IoError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self
                .script
                .lock()
                .expect("script mutex poisoned")
                .pop_front()
                .expect("create called more times than scripted")
            {
                Some(kind) => Err(IoError::from(kind)),
                None => Ok(()),
            }
        }
    }

    #[tokio::test]
    async fn create_run_erofs_cures_once_then_succeeds() {
        let force_cure_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repair: Arc<dyn RepairLane> = Arc::new(RecordingRepairLane {
            force_cure_calls: force_cure_calls.clone(),
        });
        let create = ScriptedCreate::new([Some(ErrorKind::ReadOnlyFilesystem), None]);
        let root = tempfile::tempdir().expect("temp root");

        let result = create_epoch_dir_with_cure(|| create.call(), Some(&repair), root.path()).await;

        assert!(result.is_ok(), "the cured retry succeeds");
        assert_eq!(
            force_cure_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "force_cure runs exactly once"
        );
        assert_eq!(create.call_count(), 2, "create runs exactly twice");
    }

    #[tokio::test]
    async fn create_run_erofs_twice_fails_loud() {
        let force_cure_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repair: Arc<dyn RepairLane> = Arc::new(RecordingRepairLane {
            force_cure_calls: force_cure_calls.clone(),
        });
        let create = ScriptedCreate::new([
            Some(ErrorKind::ReadOnlyFilesystem),
            Some(ErrorKind::ReadOnlyFilesystem),
        ]);
        let root = tempfile::tempdir().expect("temp root");

        let err = create_epoch_dir_with_cure(|| create.call(), Some(&repair), root.path())
            .await
            .expect_err("a second EROFS on the retry fails loud");

        assert_eq!(err.kind(), ErrorKind::ReadOnlyFilesystem);
        assert_eq!(
            force_cure_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the cure never runs a second time"
        );
        assert_eq!(create.call_count(), 2, "no third create past the one retry");
    }

    #[tokio::test]
    async fn create_run_erofs_local_no_cure_fails_loud() {
        let create = ScriptedCreate::new([Some(ErrorKind::ReadOnlyFilesystem)]);
        let root = tempfile::tempdir().expect("temp root");

        let err = create_epoch_dir_with_cure(|| create.call(), None, root.path())
            .await
            .expect_err("local mode has no lane to cure with");

        assert_eq!(err.kind(), ErrorKind::ReadOnlyFilesystem);
        assert_eq!(create.call_count(), 1, "no retry without a repair lane");
    }

    #[tokio::test]
    async fn create_run_non_erofs_no_cure() {
        let force_cure_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repair: Arc<dyn RepairLane> = Arc::new(RecordingRepairLane {
            force_cure_calls: force_cure_calls.clone(),
        });
        let create = ScriptedCreate::new([Some(ErrorKind::PermissionDenied)]);
        let root = tempfile::tempdir().expect("temp root");

        let err = create_epoch_dir_with_cure(|| create.call(), Some(&repair), root.path())
            .await
            .expect_err("a non-EROFS error is never cured");

        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert_eq!(
            force_cure_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the cure never runs for a non-EROFS error"
        );
        assert_eq!(create.call_count(), 1, "no retry for a non-EROFS error");
    }
}
