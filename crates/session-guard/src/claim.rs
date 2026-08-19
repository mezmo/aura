//! Claim-file surface: naming, wire body, liveness evidence, holder views.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::identity::{InstanceId, SessionId, TurnId};

/// A claim's generation. Each successful admission or steal is the next
/// generation; claim files are named with it, so every claim is unique and
/// additive — an owner only ever removes its own.
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

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Monotonic heartbeat counter — the liveness measure inside a claim.
/// Never wall-clock: staleness is "the sequence stopped advancing", so no
/// clock skew between pods can fake or mask liveness.
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

/// One observation of a claim's heartbeat, taken by this pod (observer-local
/// time). Two samples with a non-advancing sequence, taken at least
/// [`crate::stale_after`] apart, are the raw material of steal evidence.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatSample {
    /// The heartbeat sequence observed.
    pub(crate) seq: HeartbeatSeq,
    /// Observer-local observation time.
    pub(crate) observed_at: Instant,
}

impl HeartbeatSample {
    /// Record a sample (adapter-side).
    pub(crate) fn new(seq: HeartbeatSeq) -> Self {
        Self {
            seq,
            observed_at: Instant::now(),
        }
    }

    /// The observed heartbeat sequence.
    #[must_use]
    pub const fn seq(self) -> HeartbeatSeq {
        self.seq
    }

    /// When this pod took the sample.
    #[must_use]
    pub const fn observed_at(self) -> Instant {
        self.observed_at
    }
}

/// Identifies one incarnation of a session's lock (the current claim file
/// name). Binds staleness evidence to a specific claim, not just a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockFingerprint(String);

impl LockFingerprint {
    /// Wrap a non-empty fingerprint (adapter-side).
    pub(crate) fn new(raw: String) -> Self {
        Self(raw)
    }
}

impl fmt::Display for LockFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Proof that a claim is stale, built by the adapter from two uncached
/// reads of the same claim at least [`crate::stale_after`] apart.
///
/// Business rule: a steal may only happen with this evidence, and the
/// evidence must name the exact claim (session + fingerprint + holder +
/// generation) whose heartbeat sequence did not advance. The public
/// constructor does not exist; only the adapter can assemble one.
#[derive(Debug, Clone)]
pub struct StalenessEvidence {
    pub(crate) session: SessionId,
    pub(crate) fingerprint: LockFingerprint,
    pub(crate) holder: InstanceId,
    pub(crate) generation: Generation,
    pub(crate) first: HeartbeatSample,
    pub(crate) second: HeartbeatSample,
}

impl StalenessEvidence {
    /// Assemble evidence (adapter-side only). Same-claim binding is by
    /// construction; `admit_with_evidence` checks the elapsed time and
    /// non-advancing sequence before acting.
    pub(crate) fn new(
        session: SessionId,
        fingerprint: LockFingerprint,
        holder: InstanceId,
        generation: Generation,
        first: HeartbeatSample,
        second: HeartbeatSample,
    ) -> Self {
        Self {
            session,
            fingerprint,
            holder,
            generation,
            first,
            second,
        }
    }

    /// The session the evidence is for.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

/// The claim-file wire body. Everything an observer needs to route or
/// evaluate liveness, nothing wall-clock based.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaimBody {
    /// Holder instance.
    pub instance: String,
    /// The turn that claimed.
    pub turn: String,
    /// Claim generation (matches the file name).
    pub generation: u64,
    /// Latest heartbeat sequence written by the holder.
    pub heartbeat_seq: u64,
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
    pub instance: InstanceId,
    /// Whether that instance is this process.
    pub locality: Locality,
    /// Last observed heartbeat, if the backing store exposes one.
    pub last_heartbeat: Option<HeartbeatSeq>,
}

/// Claim file path: `{root}/{session}/{generation}-{turn}.CLAIM`. Unique per
/// admission by construction — no claim file is ever rewritten in place.
#[must_use]
pub fn claim_path(
    root: &Path,
    session: &SessionId,
    generation: Generation,
    turn: TurnId,
) -> PathBuf {
    root.join(session.as_ref())
        .join(format!("{generation}-{turn}.CLAIM"))
}
