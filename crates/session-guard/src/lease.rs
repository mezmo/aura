//! Liveness: the heartbeat lease, the self-fence deadline, and the write
//! capability they gate.
//!
//! Two roles are split by type so they cannot be confused:
//!
//! - [`Liveness`] is a clone-safe, drop-safe *observation* of the lease
//!   state (an atomic flag). Capabilities hold clones; dropping or
//!   cloning one never changes the state.
//! - [`Revocation`] is the *authority* to end the lease. Exactly the
//!   lease itself and the actor's [`ActorExitGuard`] hold one; dropping
//!   either revokes. Misuse fails safe: an exit guard dropped outside
//!   the actor task revokes immediately (lease reads Lost), never
//!   Live-after-death.
//!
//! The third liveness leg is the [`SelfFenceDeadline`] (codex M9): the
//! holder's conservative local view of when the server-side lease dies,
//! re-anchored at every beat *transmission* (`transmit + ttl − margin`),
//! so a wedged renewal path fails capabilities closed even before a
//! revocation lands. Server-side lease truth lives in Postgres
//! (`clock_timestamp()` — invariant I6); the deadline only ever makes a
//! holder stop *earlier* than the authority, never later.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::task::JoinHandle;

use crate::epoch::Epoch;
use crate::identity::{HolderId, SessionId, TurnId};

/// Live state of a held claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeaseState {
    /// Heartbeats are advancing; the claim is live.
    #[default]
    Live,
    /// The lease ended (stopped, dropped, actor exited, self-fence
    /// passed, or lost); writes gated on it must stop.
    Lost,
}

/// The claim lease was lost (steal superseded us, renewal failed, the
/// self-fence deadline passed, or the actor died). The turn must
/// quarantine and stop writing.
#[derive(Debug, thiserror::Error)]
#[error("session claim lease lost (epoch {epoch}, holder {holder})")]
pub struct LeaseLost {
    /// The epoch that was lost.
    pub epoch: Epoch,
    /// The acquire attempt that held it.
    pub holder: HolderId,
    /// The session it belonged to.
    pub session: SessionId,
}

/// The server-anchored lease deadline as Postgres computed it
/// (`lease_expires_at`, a `clock_timestamp()` product). Never compared
/// against a pod clock: pod clocks affect only self-fence *timeliness*
/// (liveness), while committed-state safety rides on server-side
/// predicates evaluated inside S1–S3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LeaseDeadline(SystemTime);

impl LeaseDeadline {
    /// Wrap a server timestamp (crate-internal: rows are the only
    /// producer).
    pub(crate) const fn new(at: SystemTime) -> Self {
        Self(at)
    }

    /// The raw timestamp.
    #[must_use]
    pub const fn as_system_time(self) -> SystemTime {
        self.0
    }
}

/// The lease's time-to-live on the server (the `lease_expires_at =
/// clock_timestamp() + ttl` term in S1/S2). Non-zero by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTtl(Duration);

impl LeaseTtl {
    /// Wrap a non-zero ttl.
    ///
    /// # Errors
    /// The raw zero duration when `ttl` is zero, for the caller to
    /// report as a config error.
    pub fn new(ttl: Duration) -> Result<Self, Duration> {
        if ttl.is_zero() {
            Err(ttl)
        } else {
            Ok(Self(ttl))
        }
    }

    /// The ttl duration.
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}

/// The safety margin subtracted from the ttl when anchoring the
/// self-fence deadline (round-trip delay plus clock-skew allowance).
/// Non-zero by construction; config additionally requires
/// `margin < ttl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfFenceMargin(Duration);

impl SelfFenceMargin {
    /// Wrap a non-zero margin.
    ///
    /// # Errors
    /// The raw zero duration when `margin` is zero, for the caller to
    /// report as a config error.
    pub fn new(margin: Duration) -> Result<Self, Duration> {
        if margin.is_zero() {
            Err(margin)
        } else {
            Ok(Self(margin))
        }
    }

    /// The margin duration.
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}

/// The holder's conservative local deadline (codex M9): re-anchored at
/// every heartbeat *transmission* to `transmit_instant + ttl − margin`,
/// so the deadline never depends on a response arriving. A `None` value
/// is an unfenced lease (local admission): never expires.
///
/// Sync `Mutex` on purpose: the critical section is one `Instant`
/// store/load, never held across an await.
#[derive(Debug, Clone)]
pub(crate) struct SelfFenceDeadline {
    shared: Arc<Mutex<Option<Instant>>>,
}

impl SelfFenceDeadline {
    /// An unfenced deadline (local admission): `is_expired` is always
    /// false; anchoring is a no-op.
    pub(crate) fn unfenced() -> Self {
        Self {
            shared: Arc::new(Mutex::new(None)),
        }
    }

    /// A fenced deadline; it starts unanchored (already expired would be
    /// wrong — the first beat anchors it), so reads before the first
    /// beat do not fail a lease that was just granted. The claim flow
    /// anchors at acquisition before any capability escapes.
    pub(crate) fn fenced() -> Self {
        Self {
            shared: Arc::new(Mutex::new(None)),
        }
    }

    /// Re-anchor at a beat transmission: the server cannot have
    /// processed the beat before it was sent, so
    /// `transmitted_at + (ttl − margin)` is a conservative expiry no
    /// matter what the response says. A `None` shared value (unfenced)
    /// stays `None`.
    pub(crate) fn anchor_at(
        &self,
        transmitted_at: Instant,
        ttl: LeaseTtl,
        margin: SelfFenceMargin,
    ) {
        let mut guard = self.shared.lock().expect("self-fence mutex poisoned");
        if guard.is_some() {
            let window = ttl.get().saturating_sub(margin.get());
            *guard = Some(transmitted_at.checked_add(window).unwrap_or(transmitted_at));
        }
    }

    /// Whether the anchored deadline has passed. An unfenced deadline
    /// never expires.
    pub(crate) fn is_expired(&self) -> bool {
        self.shared
            .lock()
            .expect("self-fence mutex poisoned")
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// Clone-safe, drop-safe liveness observation: a shared atomic flag.
/// Holding or dropping a `Liveness` never alters state; only a
/// [`Revocation`] can.
#[derive(Debug, Clone)]
pub(crate) struct Liveness {
    lost: Arc<AtomicBool>,
}

impl Liveness {
    /// Whether the lease is Lost. Authoritative: every end path sets the
    /// flag before anything else.
    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
}

/// The authority to end a lease. Held only by [`HeartbeatLease`] and the
/// actor's [`ActorExitGuard`]; dropping either revokes. Revoking also
/// wakes the heartbeat loop (cooperative shutdown).
#[derive(Debug, Clone)]
pub(crate) struct Revocation {
    liveness: Liveness,
    notify: Arc<tokio::sync::Notify>,
}

impl Revocation {
    /// A live revocation bound to a fresh liveness flag.
    pub(crate) fn new() -> Self {
        Self {
            liveness: Liveness {
                lost: Arc::new(AtomicBool::new(false)),
            },
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Mark lost and wake the heartbeat loop. Idempotent.
    pub(crate) fn revoke(&self) {
        self.liveness.lost.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// A clone-safe observation of this revocation's flag.
    pub(crate) fn liveness(&self) -> Liveness {
        self.liveness.clone()
    }

    /// Wait until revoked (heartbeat loop shutdown arm). The `Notified`
    /// future is created before the lost check so a `revoke()` after
    /// registration wakes it, while a `revoke()` before registration is
    /// caught by the lost check.
    pub(crate) async fn revoked(&self) {
        let notified = self.notify.notified();
        if self.liveness.is_lost() {
            return;
        }
        notified.await;
    }
}

impl Drop for Revocation {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Owned by the heartbeat actor task: revokes when the task exits for
/// any reason (return, panic, abort), so actor death can never leave a
/// lease Live. Dropping it *outside* the task also revokes — misuse
/// fails toward Lost, never toward a false Live.
#[derive(Debug)]
pub(crate) struct ActorExitGuard {
    revocation: Revocation,
}

impl ActorExitGuard {
    /// Arm the exit guard for a starting actor.
    pub(crate) fn new(revocation: Revocation) -> Self {
        Self { revocation }
    }
}

impl Drop for ActorExitGuard {
    fn drop(&mut self) {
        self.revocation.revoke();
    }
}

/// The identity a lease (and everything derived from it) is bound to.
/// Constructed only at [`crate::state::AcquiredClaim`]'s lease-building
/// site, so lease, capability, and release all share one claim identity.
#[derive(Debug, Clone)]
pub(crate) struct ClaimLeaseSource {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) epoch: Epoch,
    pub(crate) holder: HolderId,
}

/// The heartbeat lease, carried by value through the turn state chain.
/// Owns the [`Revocation`], so the lease's end (stop, drop) is the
/// lease's revocation.
#[derive(Debug)]
pub struct HeartbeatLease {
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    deadline: SelfFenceDeadline,
    revocation: Revocation,
    actor: Option<JoinHandle<()>>,
}

impl HeartbeatLease {
    /// A lease with no background actor (local admission): the
    /// revocation flag alone decides liveness, and the self-fence is
    /// unfenced.
    pub(crate) fn static_from(source: ClaimLeaseSource) -> Self {
        Self {
            session: source.session,
            turn: source.turn,
            epoch: source.epoch,
            holder: source.holder,
            deadline: SelfFenceDeadline::unfenced(),
            revocation: Revocation::new(),
            actor: None,
        }
    }

    /// A lease whose heartbeat loop the lease itself owns and drives.
    /// `write` performs one S2 renewal; the loop anchors the self-fence
    /// deadline at every beat *transmission* (before the write is
    /// awaited), then calls it every `beat` while live. Any write
    /// failure revokes and ends the loop (renewal failure fails closed:
    /// Lost and PG-down are indistinguishable by design — I5).
    ///
    /// Shutdown is cooperative so release ordering holds:
    /// [`stop`](Self::stop) revokes, wakes the loop, and *joins* it — an
    /// in-flight renewal completes before `stop` returns, so no renewal
    /// can land after release begins. The drop path (abandonment) aborts
    /// instead and the documented residual window applies.
    pub(crate) fn with_heartbeat<F, Fut>(
        source: ClaimLeaseSource,
        beat: BeatInterval,
        ttl: LeaseTtl,
        margin: SelfFenceMargin,
        write: F,
    ) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), std::io::Error>> + Send + 'static,
    {
        let revocation = Revocation::new();
        let guard = ActorExitGuard::new(revocation.clone());
        let shutdown = revocation.clone();
        let deadline = SelfFenceDeadline::fenced();
        let loop_deadline = deadline.clone();
        let mut write = write;
        let actor = tokio::spawn(async move {
            let _guard = guard;
            loop {
                tokio::select! {
                    _ = shutdown.revoked() => break,
                    _ = tokio::time::sleep(beat.get()) => {
                        if shutdown.liveness().is_lost() {
                            break;
                        }
                        // Anchor at transmission (M9): the conservative
                        // deadline must not wait on the response.
                        loop_deadline.anchor_at(Instant::now(), ttl, margin);
                        if write().await.is_err() {
                            shutdown.revoke();
                            break;
                        }
                    }
                }
            }
        });
        Self {
            session: source.session,
            turn: source.turn,
            epoch: source.epoch,
            holder: source.holder,
            deadline,
            revocation,
            actor: Some(actor),
        }
    }

    /// The epoch this lease defends.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The acquire attempt this lease defends.
    #[must_use]
    pub const fn holder(&self) -> HolderId {
        self.holder
    }

    /// Current liveness: the revocation flag and the self-fence deadline.
    #[must_use]
    pub fn state(&self) -> LeaseState {
        if self.revocation.liveness.is_lost() || self.deadline.is_expired() {
            LeaseState::Lost
        } else {
            LeaseState::Live
        }
    }

    /// A write capability bound to this lease's full identity. Cheap to
    /// clone and drop: capabilities observe liveness, they never revoke.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        WriteCapability {
            session: self.session.clone(),
            turn: self.turn,
            epoch: self.epoch,
            holder: self.holder,
            liveness: self.revocation.liveness.clone(),
            deadline: self.deadline.clone(),
        }
    }

    /// Ordered lease shutdown: revoke first (every capability fails
    /// closed from this instant), wake the heartbeat loop, and join it.
    /// Joining — not aborting — is what guarantees an in-flight renewal
    /// completes before release begins.
    pub(crate) async fn stop(mut self) {
        self.revocation.revoke();
        if let Some(actor) = self.actor.take() {
            let _ = actor.await;
        }
    }
}

impl Drop for HeartbeatLease {
    fn drop(&mut self) {
        // Abandonment path: revoke synchronously (also covered by
        // Revocation's own Drop); abort without joining.
        self.revocation.revoke();
        if let Some(actor) = self.actor.take() {
            actor.abort();
        }
    }
}

/// Permission to write on behalf of one turn of one session, under one
/// `(epoch, holder)` claim identity. Identity-complete: a capability can
/// be checked against the exact write target, not just a bare token.
/// Fails closed once the backing lease revokes *or* the self-fence
/// deadline passes; cloning and dropping a capability never changes
/// lease state.
#[derive(Debug, Clone)]
pub struct WriteCapability {
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    liveness: Liveness,
    deadline: SelfFenceDeadline,
}

impl WriteCapability {
    /// The session this capability authorizes writes for.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The turn this capability belongs to.
    #[must_use]
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// The claim epoch.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The acquire attempt.
    #[must_use]
    pub const fn holder(&self) -> HolderId {
        self.holder
    }

    /// Fail if the backing lease is no longer live — revoked *or*
    /// self-fence deadline passed.
    ///
    /// # Errors
    /// [`LeaseLost`] naming this capability's identity when the lease is
    /// gone.
    pub fn assert_live(&self) -> Result<(), LeaseLost> {
        if self.liveness.is_lost() || self.deadline.is_expired() {
            Err(LeaseLost {
                epoch: self.epoch,
                holder: self.holder,
                session: self.session.clone(),
            })
        } else {
            Ok(())
        }
    }
}

/// How often the heartbeat actor beats, derived from config. Non-zero by
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeatInterval(Duration);

impl BeatInterval {
    /// Wrap a non-zero interval.
    ///
    /// # Errors
    /// The raw zero duration when `interval` is zero, for the caller to
    /// report as a config error.
    pub fn new(interval: Duration) -> Result<Self, Duration> {
        if interval.is_zero() {
            Err(interval)
        } else {
            Ok(Self(interval))
        }
    }

    /// The interval duration.
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}
