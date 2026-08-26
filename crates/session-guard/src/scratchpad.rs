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

use tokio::io::AsyncWriteExt;

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
    pub fn parse(raw: &str) -> Result<Self, InvalidScratchpadName> {
        if raw.is_empty() {
            return Err(InvalidScratchpadName {
                reason: "empty name".into(),
            });
        }
        if raw.len() > 255 {
            return Err(InvalidScratchpadName {
                reason: "name exceeds 255 bytes".into(),
            });
        }
        if raw == "." || raw == ".." {
            return Err(InvalidScratchpadName {
                reason: "dot component".into(),
            });
        }
        if raw.contains('/') {
            return Err(InvalidScratchpadName {
                reason: "path separator".into(),
            });
        }
        if raw.contains('\0') {
            return Err(InvalidScratchpadName {
                reason: "interior NUL".into(),
            });
        }
        Ok(Self(raw.to_string()))
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
    pub async fn write_scratchpad(
        &self,
        name: &ScratchpadName,
        bytes: &[u8],
    ) -> Result<(), ScratchpadError> {
        self.capability().assert_live()?;
        let dir = self.scratch_dir();
        let target = dir.join(name.as_ref());
        // A per-write nonce keeps two concurrent scratch writes from
        // colliding on the temp path before either renames.
        let tmp = dir.join(format!(".sg-scratch-tmp-{}", uuid::Uuid::now_v7()));
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, &target).await?;
        let dir_handle = tokio::fs::File::open(dir).await?;
        dir_handle.sync_all().await?;
        Ok(())
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
    pub async fn read_scratchpad(&self, name: &ScratchpadName) -> Result<Vec<u8>, ScratchpadError> {
        self.capability().assert_live()?;
        let target = self.scratch_dir().join(name.as_ref());
        match tokio::fs::read(&target).await {
            Ok(bytes) => Ok(bytes),
            // A stale pointer into a prior (or never-written) turn's
            // scratch reads as ENOENT; surface it loud, never as empty.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(ScratchpadError::NotFound {
                    turn: self.capability().turn(),
                    name: name.clone(),
                })
            }
            Err(err) => Err(err.into()),
        }
    }
}
