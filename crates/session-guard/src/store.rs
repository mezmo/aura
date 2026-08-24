//! The claim-store port: the claims table's five statements as typed
//! outcomes, plus the statements themselves.
//!
//! Postgres is the sole claim authority (invariant I5: PG-down fails the
//! claim path, never degrades). Every mutation predicates on the fence
//! triple `(session_id, epoch, holder_id)` plus the server-side lease
//! clock (`clock_timestamp()` — invariant I6), so a stale holder's
//! heartbeat, commit, or release matches zero rows even across a
//! failover that regresses the epoch (invariant I2). The table is
//! never-DELETE; READ COMMITTED (the default) is pinned, because the
//! steal's `WHERE`-re-evaluation after the row-lock wait is documented
//! READ COMMITTED behavior (invariant I7).
//!
//! The trait is crate-internal: the public port is
//! [`crate::TurnAdmission`]. Layer-2 golden tests drive a scripted
//! `ClaimStore` double, so the turn lifecycle is testable without a
//! database.

use async_trait::async_trait;

use crate::claim::{ClaimOutcome, HolderView};
use crate::epoch::Epoch;
use crate::identity::{HolderId, OpId, PodId, SessionId, TurnId};
use crate::lease::{LeaseDeadline, LeaseTtl};
use crate::manifest::Manifest;

/// The fence identity every mutation predicates on (I2). `turn` rides
/// along so the park latch can be derived (`parked_turn` is always the
/// *committing* turn's id — never a free parameter).
#[derive(Debug, Clone)]
pub(crate) struct ClaimRef {
    pub(crate) session: SessionId,
    pub(crate) turn: crate::identity::TurnId,
    pub(crate) epoch: Epoch,
    pub(crate) holder: HolderId,
}

/// A claim attempt: S1's inputs. `holder` is minted fresh for the
/// attempt; `turn` doubles as the reify key (the parked predicate
/// compares `parked_turn = turn`).
#[derive(Debug, Clone)]
pub(crate) struct ClaimRequest {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) holder: HolderId,
    pub(crate) pod: PodId,
    pub(crate) ttl: LeaseTtl,
}

/// The store is unreachable or errored. Fail-stop (I5): mapped to
/// [`crate::AdmissionError::StoreUnavailable`] at the admission port.
/// Payload is diagnostic-only; nothing branches on it.
#[derive(Debug, thiserror::Error)]
#[error("claim store unavailable: {0}")]
pub struct StoreUnavailable(String);

impl StoreUnavailable {
    /// Wrap a driver error's display (crate-internal).
    pub(crate) fn msg(detail: impl ToString) -> Self {
        Self(detail.to_string())
    }
}

/// S2's outcome.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HeartbeatOutcome {
    /// The lease was extended; carries the new server-side deadline.
    Renewed {
        /// The row's new `lease_expires_at`.
        deadline: LeaseDeadline,
    },
    /// Zero rows matched: the claim was stolen or expired. The lease
    /// revokes.
    Lost,
}

/// S3's outcome.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CommitOutcome {
    /// The manifest, op id, and park latch landed.
    Committed,
    /// Zero rows matched: the fence triple no longer holds. Quarantine
    /// and fail loud.
    LostFence,
}

/// S4's outcome. Release is idempotent: supersession is not an error.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ReleaseOutcome {
    /// The lease was expired-now.
    Released,
    /// Zero rows matched: the claim was already superseded; the new
    /// owner stands.
    Superseded,
}

/// The commit-unknown reconciliation verdict (B3): a read-back on
/// `last_commit_op` *and* the fence triple, never a blind re-UPDATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitDisposition {
    /// The row still shows our fence triple and carries this op id:
    /// the commit landed.
    Applied,
    /// The row still shows our fence triple but a different op id: our
    /// commit definitively did not land; retrying or aborting is safe.
    NotApplied,
    /// The row shows a *different* epoch/holder: we were superseded, and
    /// whether our commit landed before the steal is unknowable from the
    /// row (the single `last_commit_op` slot may have been overwritten
    /// by the new holder). Honest indeterminacy, reported as such — the
    /// turn's delivery is moot once stolen, so the caller treats this as
    /// a lost turn, not a retryable one.
    SupersededUnknown,
}

/// Whether one commit latches the session as parked. The latch value
/// itself is never a parameter: `Parked` writes the *committing* turn's
/// id, so a contradictory kind/latch combination is unrepresentable
/// (codex B5's "nothing enforces same-TurnId reification" starts here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkLatch {
    /// A normal commit: `parked_turn` is written NULL.
    NotParked,
    /// A park commit (HITL-271's driver): `parked_turn` is written with
    /// the committing turn's id.
    Parked,
}

/// The claim-store port: one method per statement.
#[async_trait]
pub(crate) trait ClaimStore: Send + Sync {
    /// S1: fresh claim, steal-after-expiry, or reify — one statement.
    /// Zero updated rows is classified by a follow-up read into
    /// `Busy` (live lease) or `Parked` (parked on a different turn).
    /// The classify read can race a release landing between S1 and it
    /// (the row reads as claimable again); the implementation retries S1
    /// once in that case rather than reporting a third refusal shape.
    async fn claim(&self, req: &ClaimRequest) -> Result<ClaimOutcome, StoreUnavailable>;

    /// S2: extend the lease. Predicated on the fence triple and an
    /// unexpired lease (codex B1), so a thawed holder cannot renew a
    /// stolen or expired claim.
    async fn heartbeat(
        &self,
        claim: &ClaimRef,
        ttl: LeaseTtl,
    ) -> Result<HeartbeatOutcome, StoreUnavailable>;

    /// S3: publish the cumulative manifest, record the op id, and write
    /// the park latch — one atomic statement (I4). Predicated on the
    /// fence triple and an unexpired lease (B1). The latch value is
    /// derived from the claim's own turn (never a free parameter).
    async fn commit(
        &self,
        claim: &ClaimRef,
        latch: ParkLatch,
        op: OpId,
        manifest: &Manifest,
    ) -> Result<CommitOutcome, StoreUnavailable>;

    /// S4: expire-now release, predicated on the fence triple. The row
    /// stays (never-DELETE, I7).
    async fn release(&self, claim: &ClaimRef) -> Result<ReleaseOutcome, StoreUnavailable>;

    /// S4 controller variant: expire every live row a pod holds (M10).
    /// Returns the number of rows expired. Administrative; the caller is
    /// the controller, not a turn path. Known residual: pod-name reuse
    /// can expire a same-named replacement pod's claims — bounded to an
    /// unnecessary bounce, never corruption (DESIGN.md risk list).
    async fn release_pod(&self, pod: &PodId) -> Result<u64, StoreUnavailable>;

    /// Commit-unknown reconciliation (B3): read back the fence triple
    /// and `last_commit_op`, then compare both.
    async fn reconcile_commit(
        &self,
        session: &SessionId,
        claim: &ClaimRef,
        op: OpId,
    ) -> Result<CommitDisposition, StoreUnavailable>;

    /// Read the current holder for HITL routing: the row only if its
    /// lease is live.
    async fn locate(&self, session: &SessionId) -> Result<Option<HolderView>, StoreUnavailable>;
}

// ---------------------------------------------------------------------------
// The statements. `$n` intervals are passed as milliseconds (float8) and
// multiplied by `interval '1 millisecond'`, because the driver's interval
// mapping is feature-gated and milliseconds are what config speaks.
// ---------------------------------------------------------------------------

/// Greenfield bootstrap (there is no migration — I7's table is created
/// by the adapter at startup if absent).
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS session_claims (
  session_id       text PRIMARY KEY,
  epoch            bigint      NOT NULL,
  holder_id        uuid        NOT NULL,
  holder_pod       text        NOT NULL,
  lease_expires_at timestamptz NOT NULL,
  manifest         jsonb       NOT NULL DEFAULT '{}',
  last_commit_op   uuid,
  parked_turn      uuid
)";

/// S1 — claim / steal / reify. Atomic under READ COMMITTED: a
/// conflicting concurrent claim holds the row lock; the loser re-evaluates
/// the WHERE against the winner's updated row and matches zero rows
/// (I7). The lease deadline is computed inside the locked update via
/// `clock_timestamp()` (B2/I6) — never from the client's clock, never
/// from before the wait. A successful update always clears the park
/// latch: either there was no park, or this claim presented the parked
/// turn (reify).
pub(crate) const S1_CLAIM: &str = "
INSERT INTO session_claims AS c (session_id, epoch, holder_id, holder_pod,
                                 lease_expires_at, manifest, last_commit_op, parked_turn)
VALUES ($1, 1, $2, $3, clock_timestamp() + ($4 * interval '1 millisecond'),
        '{}', NULL, NULL)
ON CONFLICT (session_id) DO UPDATE
  SET epoch            = c.epoch + 1,
      holder_id        = EXCLUDED.holder_id,
      holder_pod       = EXCLUDED.holder_pod,
      lease_expires_at = clock_timestamp() + ($4 * interval '1 millisecond'),
      parked_turn      = NULL
  WHERE c.lease_expires_at < clock_timestamp()
    AND (c.parked_turn IS NULL OR c.parked_turn = $5)
RETURNING c.epoch, c.lease_expires_at, c.manifest";
// $1 text session_id · $2 uuid holder_id · $3 text holder_pod
// $4 float8 ttl millis · $5 uuid turn

/// S1 classify — run only when S1 updates zero rows, to tell the two
/// refusal shapes apart honestly: `Parked` (parked on a different turn;
/// unbounded wait, no retry hint) vs `Busy` (live lease; hint from
/// config — codex M7).
pub(crate) const S1_CLASSIFY: &str = "
SELECT holder_pod, lease_expires_at, parked_turn
FROM session_claims
WHERE session_id = $1";

/// S2 — heartbeat. The lease predicate (B1) is what makes staleness ≡
/// lease expiry: after expiry the row matches zero rows, so a thawed
/// holder can never renew its way past a steal.
pub(crate) const S2_HEARTBEAT: &str = "
UPDATE session_claims
SET lease_expires_at = clock_timestamp() + ($4 * interval '1 millisecond')
WHERE session_id = $1 AND epoch = $2 AND holder_id = $3
  AND lease_expires_at > clock_timestamp()
RETURNING lease_expires_at";
// $1 text · $2 bigint epoch · $3 uuid holder_id · $4 float8 ttl millis

/// S3 — commit. One atomic statement publishes the manifest, records
/// the logical op id, and writes the park latch (I4). The lease
/// predicate (B1) refuses commits from a holder whose lease already
/// expired — the store-side fence that replaces any filesystem fence.
/// Parameters are contiguous; $6 is `Some(claim.turn)` when parking,
/// NULL otherwise (derived from [`ParkLatch`], never free).
pub(crate) const S3_COMMIT: &str = "
UPDATE session_claims
SET manifest = $4,
    last_commit_op = $5,
    parked_turn = $6
WHERE session_id = $1 AND epoch = $2 AND holder_id = $3
  AND lease_expires_at > clock_timestamp()";
// $1 text · $2 bigint epoch · $3 uuid holder_id · $4 jsonb manifest ·
// $5 uuid op · $6 uuid-or-null parked_turn (the committing turn when parking)

/// S4 — release (expire-now). Idempotent by predicate: supersession
/// matches zero rows and is not an error. The controller variant for
/// pod-death cleanup predicates on `holder_pod` instead.
pub(crate) const S4_RELEASE: &str = "
UPDATE session_claims
SET lease_expires_at = clock_timestamp()
WHERE session_id = $1 AND epoch = $2 AND holder_id = $3";

/// S4 controller variant — release whatever a dead pod holds (M10).
pub(crate) const S4_RELEASE_POD: &str = "
UPDATE session_claims
SET lease_expires_at = clock_timestamp()
WHERE holder_pod = $1 AND lease_expires_at > clock_timestamp()";

/// S5 — commit-unknown reconciliation read (B3). Reads the fence triple
/// alongside the op slot so a supersession in between is reported as
/// [`CommitDisposition::SupersededUnknown`], not a false `NotApplied`.
pub(crate) const S5_RECONCILE: &str = "
SELECT epoch, holder_id, last_commit_op FROM session_claims
WHERE session_id = $1";

/// S6 — holder lookup for HITL routing: only a live lease reports a
/// holder.
pub(crate) const S6_LOCATE: &str = "
SELECT holder_pod, lease_expires_at FROM session_claims
WHERE session_id = $1 AND lease_expires_at > clock_timestamp()";
