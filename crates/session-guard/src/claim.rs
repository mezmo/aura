//! Claim domain types: what the authority returns and what admission
//! reports.
//!
//! There is no claim file. The election is S1's single
//! `INSERT .. ON CONFLICT DO UPDATE .. WHERE lease_expired` against the
//! claims row (see [`crate::store`]); the row is never deleted, so the
//! epoch is the session's monotonic fencing token. This module carries
//! only the *outcome* vocabulary — the statements and the port live in
//! `store.rs`, the consuming state chain in `state.rs`.

use std::time::Duration;

use crate::epoch::Epoch;
use crate::identity::{HolderId, PodId, SessionId, TurnId};
use crate::lease::LeaseDeadline;
use crate::manifest::Manifest;

/// The claims row as read from Postgres (the authority's view).
/// Crate-internal: consumers see [`ClaimOutcome`] or [`HolderView`],
/// never the raw row.
#[derive(Debug, Clone)]
pub(crate) struct ClaimsRow {
    pub(crate) session: SessionId,
    pub(crate) epoch: Epoch,
    pub(crate) holder: HolderId,
    pub(crate) holder_pod: PodId,
    pub(crate) lease_expires_at: LeaseDeadline,
    pub(crate) manifest: Manifest,
    pub(crate) last_commit_op: Option<crate::identity::OpId>,
    pub(crate) parked_turn: Option<TurnId>,
}

/// The outcome of one claim attempt (S1, plus the classify read when S1
/// updates zero rows).
#[derive(Debug)]
pub(crate) enum ClaimOutcome {
    /// The claim was granted (fresh, stolen after lease expiry, or a
    /// reify that matched the park latch).
    Granted(GrantedClaim),
    /// A live claim exists. The retry hint is the configured fixed hint
    /// (codex M7: a zero-row S1 returns no deadline to derive one from).
    Busy(BusyClaim),
    /// The session is parked awaiting an external driver (HITL) on a
    /// different turn. Not retryable on a hint — the wait is unbounded;
    /// routing surfaces it honestly.
    Parked(ParkedClaim),
}

/// A granted claim: everything the lease and the lock are built from.
/// The manifest rides the claim (Q7: manifest lives in the row), so the
/// G2 handshake needs no filesystem read at claim time. `granted_at` is
/// the adapter's instant of S1 *transmission* — the self-fence's initial
/// anchor (codex M9), so the pre-first-beat window is fenced too.
#[derive(Debug)]
pub(crate) struct GrantedClaim {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) epoch: Epoch,
    pub(crate) holder: HolderId,
    pub(crate) pod: PodId,
    pub(crate) lease_expires_at: LeaseDeadline,
    pub(crate) manifest: Manifest,
    pub(crate) granted_at: std::time::Instant,
}

/// A live-claim refusal.
#[derive(Debug)]
pub(crate) struct BusyClaim {
    /// Who holds the session (pod identity + observed lease deadline).
    pub(crate) holder: HolderView,
    /// How long the caller should wait before retrying (configured).
    pub(crate) retry_after: Duration,
}

/// A parked refusal: the session waits on the named parked turn.
#[derive(Debug)]
pub(crate) struct ParkedClaim {
    /// The turn the session is parked on.
    pub(crate) turn: TurnId,
}

/// Who holds a session, as an honest observation can report it. Opaque:
/// constructed only inside this crate (`here` for a local hold — no
/// store read needed; `remote` for one row read), so no caller can pair
/// a pod with a lease deadline it never observed. The `remote`
/// constructor takes both row fields at once so they cannot be paired
/// from different reads by construction site (a crate-internal contract,
/// Layer-2 pinned — same class as the release closure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderView {
    pod: PodId,
    locality: Locality,
    lease_expires_at: Option<LeaseDeadline>,
}

impl HolderView {
    /// A local hold: this process holds the session.
    pub(crate) fn here(pod: PodId) -> Self {
        Self {
            pod,
            locality: Locality::Here,
            lease_expires_at: None,
        }
    }

    /// A remote hold, per one row read: pod and deadline arrive together.
    pub(crate) fn remote(pod: PodId, lease_expires_at: LeaseDeadline) -> Self {
        Self {
            pod,
            locality: Locality::Remote,
            lease_expires_at: Some(lease_expires_at),
        }
    }

    /// The pod holding the session, local or remote.
    #[must_use]
    pub fn pod(&self) -> &PodId {
        &self.pod
    }

    /// Whether the holder is this process.
    #[must_use]
    pub const fn locality(&self) -> Locality {
        self.locality
    }

    /// The observed server-side lease deadline (present only for remote
    /// observations).
    #[must_use]
    pub const fn lease_expires_at(&self) -> Option<LeaseDeadline> {
        self.lease_expires_at
    }
}

/// Whether a session's holder is this process or another instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    /// Held by this process.
    Here,
    /// Held by another instance.
    Remote,
}
