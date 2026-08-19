//! Claim surface: the single well-known claim per session, its wire body,
//! validated observations, and liveness evidence.
//!
//! One session has ONE claim file at a fixed path (`{root}/{session}/CLAIM`)
//! created atomically with `O_EXCL` — that is the cross-instance election.
//! The body carries the current generation and heartbeat sequence; a steal
//! replaces the body (tmp + rename) only after validated
//! [`StalenessEvidence`]; a release renames the claim to a unique tombstone
//! name and unlinks that, so no client ever deletes a name it does not own
//! the current generation of without a re-read between (residual window
//! named in DESIGN.md).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::identity::{InstanceId, SessionId, TurnId};

/// A claim's generation. Each successful admission or steal is the next
/// generation. Body and election agree on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(u64);

impl Generation {
    /// Wrap an already-known generation number.
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The generation that follows this one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// The raw generation number.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Monotonic heartbeat counter — the liveness measure inside a claim.
/// Never wall-clock: staleness is "the sequence stopped advancing", so no
/// clock skew between instances can fake or mask liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HeartbeatSeq(u64);

impl HeartbeatSeq {
    /// Wrap an already-known sequence number.
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The next sequence number.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
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

/// The unique tombstone path for one generation of a session's claim:
/// `{root}/{session}/{generation}.TOMBSTONE`. Renaming the live claim here
/// is the release; the tombstone unlink can only hit this generation.
#[must_use]
pub fn tombstone_path(root: &Path, session: &SessionId, generation: Generation) -> PathBuf {
    root.join(session.as_ref())
        .join(format!("{generation}.TOMBSTONE"))
}

/// Wire body of a claim file. Private: parsing goes through
/// [`ObservedClaim::from_wire`], which binds it to the file it was read
/// from and constrains every field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClaimWire {
    instance: String,
    turn: String,
    generation: u64,
    heartbeat_seq: u64,
}

impl ClaimWire {
    fn initial(instance: &InstanceId, turn: TurnId, generation: Generation) -> Self {
        Self {
            instance: instance.as_ref().to_owned(),
            turn: turn.to_string(),
            generation: generation.as_u64(),
            heartbeat_seq: HeartbeatSeq::new(0).as_u64(),
        }
    }
}

/// One complete, validated observation of a session's claim, from a single
/// uncached read. Everything downstream (holder views, staleness
/// evidence, steal revalidation) starts from one of these; nothing pairs
/// raw samples with remembered metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedClaim {
    session: SessionId,
    holder: InstanceId,
    turn: TurnId,
    generation: Generation,
    heartbeat: HeartbeatSeq,
    observed_at: Instant,
}

impl ObservedClaim {
    /// Parse a wire body as an observation of `session`'s claim. Fallible:
    /// holder and turn must parse, generation and heartbeat must be
    /// non-degenerate.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub(crate) fn from_wire(
        session: SessionId,
        wire: ClaimWire,
    ) -> Result<Self, crate::claim::WireError> {
        todo!("fill: validate + bind; aura #421 follow-up")
    }

    /// Serialize this observation's wire form (heartbeat renewal writes).
    pub(crate) fn to_wire(&self) -> ClaimWire {
        todo!("fill: field mapping; aura #421 follow-up")
    }

    /// The holder named by the observed claim.
    #[must_use]
    pub fn holder(&self) -> &InstanceId {
        &self.holder
    }

    /// The observed generation.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// The observed heartbeat.
    #[must_use]
    pub const fn heartbeat(&self) -> HeartbeatSeq {
        self.heartbeat
    }

    /// Observer-local read time.
    #[must_use]
    pub const fn observed_at(&self) -> Instant {
        self.observed_at
    }
}

/// Why a wire body is not a valid observation. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid claim wire: {0}")]
pub struct WireError(String);

/// Why two observations do not prove staleness. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    /// The observations are of different claims (holder, turn, or
    /// generation changed between reads) — the holder was live.
    #[error("claim changed between observations")]
    ClaimChanged,
    /// The heartbeat advanced — the holder is live.
    #[error("heartbeat advanced between observations")]
    HeartbeatAdvanced,
    /// The observations are closer together than `stale_after`; a steal
    /// on this evidence would race a live-but-slow renewal.
    #[error("observations not separated by the staleness window")]
    TooCloseTogether,
}

/// Proof that one exact claim (same session, holder, turn, generation)
/// had a non-advancing heartbeat across two observations at least
/// `stale_after` apart. Assembled only by the adapter's sampler from
/// [`ObservedClaim`] pairs; there is no public constructor, and the steal
/// path revalidates against a fresh read before acting.
#[derive(Debug, Clone)]
pub(crate) struct StalenessEvidence {
    session: SessionId,
    stale_generation: Generation,
    first: ObservedClaim,
    second: ObservedClaim,
}

impl StalenessEvidence {
    /// Build evidence from two observations of the same claim. Verifies
    /// same-claim binding, non-advancing heartbeat, and window separation.
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

    /// The generation the steal would supersede.
    #[must_use]
    pub const fn stale_generation(&self) -> Generation {
        self.stale_generation
    }
}

/// Where the current holder of a session runs, for routing and honest
/// `Busy` hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    /// Held by this process (the arbiter already knows).
    Here,
    /// Held by another instance.
    Remote,
}

/// A fresh view of who holds a session. The HITL routing answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderView {
    /// Holder instance identity.
    pub holder: InstanceId,
    /// Whether that instance is this process.
    pub locality: Locality,
    /// Last observed heartbeat (`None` when assembled without a claim
    /// read, e.g. from this process's own held lock).
    pub last_heartbeat: Option<HeartbeatSeq>,
}
