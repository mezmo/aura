//! The persisted retention deadline of a parked checkpoint.
//!
//! Retention is disk evidence lifetime, distinct from every approval's
//! per-call decision deadline: an absolute stamp computed once per successful
//! checkpoint publication from the publication timestamp plus the validated
//! retention age, renewed by each re-park. Sweeps delete only strictly past
//! it, under a run reservation, with no execution active.
#![allow(dead_code)] // `from_publication`'s consumer is the E5 fill: the
// publication-transaction stamp and re-park renewal — the deadline type
// itself is stamped onto every checkpoint document

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use aura_config::ParkTtl;

/// An absolute retention deadline: the instant after which a run's parked
/// evidence may be reclaimed, computed by checked addition only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetentionExpiresAt(DateTime<Utc>);

impl RetentionExpiresAt {
    /// Stamp the deadline from a publication timestamp and the validated
    /// retention age, by checked conversion and addition: an age that cannot
    /// be represented as a deadline is refused, never wrapped or saturated.
    #[must_use]
    pub fn from_publication(published_at: DateTime<Utc>, ttl: ParkTtl) -> Option<Self> {
        let secs = i64::try_from(ttl.as_secs()).ok()?;
        published_at
            .checked_add_signed(chrono::Duration::seconds(secs))
            .map(Self)
    }

    /// Wrap an already-resolved absolute deadline.
    #[must_use]
    pub fn from_datetime(deadline: DateTime<Utc>) -> Self {
        Self(deadline)
    }

    /// The absolute deadline.
    #[must_use]
    pub fn as_datetime(&self) -> DateTime<Utc> {
        self.0
    }
}
