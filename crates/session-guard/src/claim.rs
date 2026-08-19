//! Claim surface: the single well-known claim per session, its wire body,
//! validated observations, and liveness evidence.
//!
//! One session has ONE claim file at a fixed path (`{root}/{session}/CLAIM`)
//! created atomically with `O_EXCL` — that is the cross-instance election.
//! The body carries the claim incarnation ([`Generation`], a UUIDv7 minted
//! per admission/steal, so ids never repeat or wrap) and a heartbeat
//! sequence. Mutation-linearity beyond fresh admission is an open Phase-1
//! litmus question (see DESIGN.md residual risks): the type surface
//! exposes no unconditional "replace" op; the steal path goes through
//! [`StalenessEvidence::revalidate`], which yields a [`ValidatedSteal`]
//! token the store consumes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::identity::{InstanceId, SessionId, TurnId};

/// A claim incarnation. Minted fresh (UUIDv7) on every admission or
/// steal: globally unique, time-ordered, never reused — tombstone names
/// derived from it cannot collide across restarts or delayed operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Generation(uuid::Uuid);

impl Generation {
    /// Mint a new incarnation id (adapter-side, at claim creation).
    pub(crate) fn mint() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Parse an incarnation id from its wire form.
    ///
    /// # Errors
    /// [`WireError`] when the string is not a UUID.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub(crate) fn parse(raw: &str) -> Result<Self, WireError> {
        todo!("fill: uuid parse; aura #421 follow-up")
    }

    /// The wire form (UUID string).
    #[must_use]
    pub fn as_wire(&self) -> String {
        self.0.to_string()
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Monotonic heartbeat counter — the liveness measure inside a claim.
/// Never wall-clock: staleness is "the sequence stopped advancing", so no
/// clock skew between instances can fake or mask liveness. Starts at zero
/// on a fresh claim; exhaustion (u64::MAX) is a typed dead end, not a
/// wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HeartbeatSeq(u64);

impl HeartbeatSeq {
    /// The initial heartbeat of a fresh claim.
    #[must_use]
    pub const fn initial() -> Self {
        Self(0)
    }

    /// Wrap an already-known sequence number.
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The next sequence number, if the counter has room.
    #[must_use]
    pub const fn try_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The raw sequence number.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// The fixed election path for a session: `{root}/{session}/CLAIM`.
/// One name, atomically created — the admission point.
#[must_use]
pub fn claim_path(root: &Path, session: &SessionId) -> PathBuf {
    root.join(session.as_ref()).join("CLAIM")
}

/// The unique tombstone path for one incarnation of a session's claim:
/// `{root}/{session}/{generation}.TOMBSTONE`. Renaming the live claim here
/// is the release; because generations never repeat, the tombstone unlink
/// can only target this incarnation.
#[must_use]
pub fn tombstone_path(root: &Path, session: &SessionId, generation: Generation) -> PathBuf {
    root.join(session.as_ref())
        .join(format!("{generation}.TOMBSTONE"))
}

/// Wire body of a claim file. Private: parsing goes through
/// [`ObservedClaim::from_wire`], which binds it to the session it was
/// read for and constrains every field. `initial` (crate-visible) is the
/// only constructor adapters may use for a fresh claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClaimWire {
    instance: String,
    turn: String,
    generation: String,
    heartbeat_seq: u64,
}

impl ClaimWire {
    /// The body of a freshly admitted claim.
    pub(crate) fn initial(instance: &InstanceId, turn: TurnId, generation: Generation) -> Self {
        Self {
            instance: instance.as_ref().to_owned(),
            turn: turn.to_string(),
            generation: generation.as_wire(),
            heartbeat_seq: HeartbeatSeq::initial().as_u64(),
        }
    }
}

/// Why a wire body is not a valid observation. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid claim wire: {0}")]
pub struct WireError(String);

/// One complete, validated observation of a session's claim, from a single
/// uncached read. Everything downstream (holder views, staleness
/// evidence, steal revalidation) starts from one of these; nothing pairs
/// raw samples with remembered metadata.
#[derive(Debug, Clone)]
pub struct ObservedClaim {
    session: SessionId,
    holder: InstanceId,
    turn: TurnId,
    generation: Generation,
    heartbeat: HeartbeatSeq,
    observed_at: std::time::Instant,
}

impl ObservedClaim {
    /// Parse a wire body as an observation of `session`'s claim.
    /// Fallible: holder, turn, and generation must all parse.
    ///
    /// # Errors
    /// [`WireError`] when any field fails to validate.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub(crate) fn from_wire(session: SessionId, wire: ClaimWire) -> Result<Self, WireError> {
        todo!("fill: validate + bind; aura #421 follow-up")
    }

    /// Serialize this observation's wire form (heartbeat renewal writes).
    pub(crate) fn to_wire(&self) -> ClaimWire {
        ClaimWire {
            instance: self.holder.as_ref().to_owned(),
            turn: self.turn.to_string(),
            generation: self.generation.as_wire(),
            heartbeat_seq: self.heartbeat.as_u64(),
        }
    }

    /// Advance the heartbeat for a renewal write (adapter-side).
    ///
    /// # Errors
    /// [`HeartbeatExhausted`] when the counter has no room left; the
    /// lease must treat that as lost.
    pub(crate) fn advance_heartbeat(&mut self) -> Result<(), HeartbeatExhausted> {
        match self.heartbeat.try_next() {
            Some(next) => {
                self.heartbeat = next;
                Ok(())
            }
            None => Err(HeartbeatExhausted),
        }
    }

    /// The session this claim was observed for.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The holder named by the observed claim.
    #[must_use]
    pub fn holder(&self) -> &InstanceId {
        &self.holder
    }

    /// The turn that claimed.
    #[must_use]
    pub fn turn(&self) -> TurnId {
        self.turn
    }

    /// The observed incarnation.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// The observed heartbeat.
    #[must_use]
    pub const fn heartbeat(&self) -> HeartbeatSeq {
        self.heartbeat
    }
}

/// The heartbeat counter ran out of room. Diagnostic-only; the lease
/// treats it as lost.
#[derive(Debug, thiserror::Error)]
#[error("heartbeat sequence exhausted")]
pub struct HeartbeatExhausted;

/// Why two observations do not prove staleness. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    /// The observations are of different claims (holder, turn, or
    /// incarnation changed between reads) — the holder was live.
    #[error("claim changed between observations")]
    ClaimChanged,
    /// The heartbeat advanced (or regressed) — the holder was live.
    #[error("heartbeat changed between observations")]
    HeartbeatChanged,
    /// The observations are closer together than the staleness window; a
    /// steal on this evidence would race a live-but-slow renewal.
    #[error("observations not separated by the staleness window")]
    TooCloseTogether,
}

/// Proof that one exact claim (same session, holder, turn, incarnation)
/// had an unchanged heartbeat across two observations at least
/// `stale_after` apart. Assembled only by the adapter's sampler;
/// [`revalidate`](Self::revalidate) turns it plus a fresh observation
/// into the [`ValidatedSteal`] token the store op consumes.
#[derive(Debug, Clone)]
pub(crate) struct StalenessEvidence {
    session: SessionId,
    first: ObservedClaim,
    second: ObservedClaim,
}

impl StalenessEvidence {
    /// Build evidence from two observations. Verifies same-claim binding,
    /// unchanged heartbeat, and window separation.
    ///
    /// # Errors
    /// [`EvidenceError`] when the observations do not prove staleness.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub(crate) fn from_observations(
        first: ObservedClaim,
        second: ObservedClaim,
        stale_after: Duration,
    ) -> Result<Self, EvidenceError> {
        todo!("fill: verify binding/window; aura #421 follow-up")
    }

    /// Revalidate against a fresh observation at steal time. The fresh
    /// read must still show the same holder, turn, incarnation, and an
    /// unchanged heartbeat; any movement means the holder is live.
    ///
    /// # Errors
    /// [`EvidenceError`] when the claim moved.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub(crate) fn revalidate(
        &self,
        fresh: &ObservedClaim,
    ) -> Result<ValidatedSteal, EvidenceError> {
        todo!("fill: compare identity fields; aura #421 follow-up")
    }
}

/// A steal authorization token: staleness evidence revalidated against a
/// fresh read. Consumed by the store's steal op (crate-internal); not
/// constructible any other way.
#[derive(Debug)]
pub(crate) struct ValidatedSteal {
    pub(crate) superseded: Generation,
}

/// Who holds a session, as an honest observation can report it. `Here`
/// and `Remote` carry exactly the data each can prove: a local hold knows
/// the holder without a disk read; a remote hold is always an
/// observation, so it carries the heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderView {
    /// Held by this process.
    Here {
        /// This process's identity.
        holder: InstanceId,
    },
    /// Held by another instance, per a fresh claim read.
    Remote {
        /// The observed holder.
        holder: InstanceId,
        /// The heartbeat seen in the same read.
        last_heartbeat: HeartbeatSeq,
    },
}
