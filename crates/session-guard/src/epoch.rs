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

use serde::{Deserialize, Serialize};

use crate::identity::SessionId;

/// A per-session monotonic fencing token. Starts at 1 (the fresh-insert
/// branch of S1); each steal bumps it inside the locked update.
/// Construction is crate-internal: outside code receives epochs from a
/// granted claim, never mints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Epoch(u64);

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

    /// Wrap the epoch read from a claims row (crate-internal: the only
    /// producer besides a granted claim).
    pub(crate) const fn from_raw(n: u64) -> Self {
        Self(n)
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
#[must_use]
pub fn epoch_dir(session_root: &Path, epoch: Epoch) -> PathBuf {
    session_root.join(epoch.dir_name())
}
