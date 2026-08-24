//! The epoch: the per-session fencing token and the epoch-partitioned
//! directory layout.
//!
//! One claim = one epoch = one run dir (`{session}/e{k}/`). The claims
//! row is never deleted and `epoch = epoch + 1` happens inside the
//! single-statement claim, so tokens are monotonic per session on one
//! Postgres primary (invariant I7). Epoch partitioning is the zombie
//! fence: a thawed stale holder can still write (no storage-level fence
//! exists — probe H5), but only into its own epoch dir, which no newer
//! manifest references and which GC's epoch-scope rule (I3) reclaims.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::identity::SessionId;

/// A per-session monotonic fencing token. Starts at 1 (the fresh-insert
/// branch of S1); each steal bumps it inside the locked update.
/// Construction is crate-internal: outside code receives epochs from a
/// granted claim, never mints them.
///
/// The SQL column is a signed `bigint`, so the database's domain
/// ceilings at `i64::MAX`; the `u64` newtype's exhaustion dead-end holds
/// within that domain (the store edge converts via `i64`).
/// Deserialization validates: an epoch read from storage is never 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Epoch(u64);

impl<'de> Deserialize<'de> for Epoch {
    /// Deserialization validates: 0 (or any value below
    /// [`Epoch::initial`]) is not a valid epoch — a corrupted or
    /// non-Rust-written row fails to load rather than smuggling epoch 0
    /// into GC scope math and fence predicates.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = u64::deserialize(d)?;
        if raw >= Epoch::initial().as_u64() {
            Ok(Self(raw))
        } else {
            Err(serde::de::Error::custom(
                "epoch below the initial epoch (1)",
            ))
        }
    }
}

impl Epoch {
    /// The epoch of a session's first claim.
    pub(crate) const fn initial() -> Self {
        Self(1)
    }

    /// The next epoch, if the counter has room (u64 exhaustion is a
    /// typed dead end, not a wrap).
    #[must_use]
    pub(crate) const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Wrap the epoch read from a claims row (crate-internal). `None`
    /// for 0: a corrupted or foreign-written row fails at the store edge
    /// rather than smuggling epoch 0 into GC scope math and fence
    /// predicates (the manifest path validates identically via
    /// `Deserialize`).
    pub(crate) const fn from_raw(n: u64) -> Option<Self> {
        if n >= Self::initial().0 {
            Some(Self(n))
        } else {
            None
        }
    }

    /// The raw token.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// The epoch's directory name within a session dir (`e{k}`).
    #[must_use]
    pub fn dir_name(self) -> String {
        format!("e{}", self.0)
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}", self.0)
    }
}

/// The session's root directory: `{root}/{session}`.
#[must_use]
pub fn session_dir(root: &Path, session: &SessionId) -> PathBuf {
    root.join(session.as_ref())
}

/// The epoch's run directory within a session root:
/// `{session_root}/e{k}`. Everything a claim writes lives under here;
/// nothing outside it is the claim's to touch.
///
/// Layout note (panel round 4, deferred finding): epoch-only
/// namespacing is safe under the v1 posture (single-primary fail-stop
/// Postgres). Under an async-failover deployment, two holders can share
/// one epoch — the recorded fix is `(epoch, holder)`-namespaced run
/// dirs. See DESIGN.md residual risk 1 and the round-4 ledger.
#[must_use]
pub fn epoch_dir(session_root: &Path, epoch: Epoch) -> PathBuf {
    session_root.join(epoch.dir_name())
}
