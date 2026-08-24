//! Turn-transient scratchpad I/O: same-turn, uncommitted, never
//! manifest-bound. Scratchpad bytes live under the epoch dir but outside
//! the declared artifact set — they are debris by construction and the
//! next claim's sweep collects them. Stale pointers into a prior turn's
//! scratchpad fail loud (the coordinator is never prompted toward them,
//! so the failure costs nothing and stays honest).
//!
//! This typed surface exists so the raw run-directory path never leaves
//! the crate (panel round 2): scratchpad writes are capability-gated and
//! own-epoch just like manifest artifacts, but they do not enter the
//! delta and never reach S3.

use crate::identity::TurnId;
use crate::lease::LeaseLost;
use crate::state::ActiveTurn;

/// A scratchpad entry name: one file-name component, no separators, no
/// `.`/`..`. (Scratchpad entries are flat files under the turn's epoch
/// dir; nested layout is not a turn-transient need.)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScratchpadName(String);

/// Why a raw string is not a [`ScratchpadName`]. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid scratchpad name: {reason}")]
pub struct InvalidScratchpadName {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

impl ScratchpadName {
    /// Parse and constrain a scratchpad name. The sole constructor.
    ///
    /// # Errors
    /// [`InvalidScratchpadName`] when the name is empty, too long, or
    /// contains a separator or dotfile component.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub fn parse(raw: &str) -> Result<Self, InvalidScratchpadName> {
        todo!("fill: single-component name rules; aura #421 follow-up")
    }
}

impl AsRef<str> for ScratchpadName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScratchpadName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why scratchpad I/O failed.
#[derive(Debug, thiserror::Error)]
pub enum ScratchpadError {
    /// The named entry does not exist (a stale pointer into a prior or
    /// never-written turn's scratchpad — the fail-loud case).
    #[error("scratchpad entry not found for turn {turn}: {name}")]
    NotFound {
        /// The turn whose scratchpad was read.
        turn: TurnId,
        /// The missing entry.
        name: ScratchpadName,
    },
    /// The lease was lost before or during the I/O.
    #[error(transparent)]
    LeaseLost(#[from] LeaseLost),
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ActiveTurn {
    /// Write one turn-transient scratchpad entry: capability asserted,
    /// own-epoch dir, temp + rename like the artifact path. Never
    /// recorded in the turn's delta, never committed.
    ///
    /// # Errors
    /// [`ScratchpadError::LeaseLost`] when the capability fails;
    /// [`ScratchpadError::Io`] on filesystem failure.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub async fn write_scratchpad(
        &self,
        name: &ScratchpadName,
        bytes: &[u8],
    ) -> Result<(), ScratchpadError> {
        todo!(
            "fill: assert_live + temp/rename under epoch dir, no delta record; aura #421 follow-up"
        )
    }

    /// Read one scratchpad entry written by this same turn. A missing
    /// entry is [`ScratchpadError::NotFound`], loud by design — stale
    /// pointers into prior turns are a ruled failure mode, not a silent
    /// empty read.
    ///
    /// # Errors
    /// [`ScratchpadError::NotFound`] when the entry does not exist;
    /// [`ScratchpadError::LeaseLost`] when the capability fails;
    /// [`ScratchpadError::Io`] on filesystem failure.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub async fn read_scratchpad(&self, name: &ScratchpadName) -> Result<Vec<u8>, ScratchpadError> {
        todo!(
            "fill: assert_live + read under epoch dir; missing → NotFound(turn, name); aura #421 follow-up"
        )
    }
}
