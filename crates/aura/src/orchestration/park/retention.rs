//! The persisted retention deadline of a parked checkpoint.
//!
//! Retention is disk evidence lifetime, distinct from every approval's
//! per-call decision deadline: an absolute stamp computed once per successful
//! checkpoint publication from the publication timestamp plus the validated
//! retention age, renewed by each re-park. Sweeps delete only strictly past
//! it, under a run reservation, with no execution active.
#![allow(dead_code)] // `from_publication`'s consumer is the E5 fill: the
// publication-transaction stamp and re-park renewal — the deadline type
// itself is stamped onto every checkpoint document, and `RetentionError`
// reaches the commit's fallible stamping seam

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use aura_config::ParkTtl;

/// An absolute retention deadline: the instant after which a run's parked
/// evidence may be reclaimed, computed by checked addition only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetentionExpiresAt(DateTime<Utc>);

/// Why a retention deadline could not be stamped from a publication
/// timestamp and a validated age. Both refusals are construction-time: an
/// age or sum that cannot be represented is rejected, never wrapped and
/// never saturated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum RetentionError {
    /// The validated age cannot be represented as a duration: chrono's
    /// seconds range is narrower than `u64`'s, and `ParkTtl` admits every
    /// nonzero `u64`.
    #[error("park retention age {secs}s is outside the representable duration range")]
    AgeOutOfRange { secs: u64 },
    /// The publication timestamp plus the age overflows the absolute time
    /// range.
    #[error("the publication timestamp plus the retention age overflows the absolute time range")]
    DeadlineOverflow,
}

impl RetentionExpiresAt {
    /// Stamp the deadline from a publication timestamp and the validated
    /// retention age, by checked conversion and addition: an age that cannot
    /// be represented as a deadline is refused, never wrapped or saturated.
    pub fn from_publication(
        published_at: DateTime<Utc>,
        ttl: ParkTtl,
    ) -> Result<Self, RetentionError> {
        let secs = ttl.as_secs();
        let seconds = i64::try_from(secs).map_err(|_| RetentionError::AgeOutOfRange { secs })?;
        let age =
            chrono::Duration::try_seconds(seconds).ok_or(RetentionError::AgeOutOfRange { secs })?;
        published_at
            .checked_add_signed(age)
            .map(Self)
            .ok_or(RetentionError::DeadlineOverflow)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// E5-R R4 guard: an age chrono cannot represent as a duration is
    /// refused as `AgeOutOfRange` — never wrapped, never saturated. `ParkTtl`
    /// admits every nonzero `u64`, and chrono's seconds range is narrower,
    /// so the seam must stay reachable.
    #[test]
    fn retention_from_publication_refuses_an_unrepresentable_age() {
        let published = Utc::now();
        let ttl = ParkTtl::try_new(u64::MAX).expect("ParkTtl admits every nonzero u64");
        assert_eq!(
            RetentionExpiresAt::from_publication(published, ttl),
            Err(RetentionError::AgeOutOfRange { secs: u64::MAX }),
        );
    }

    /// E5-R R4 guard: a publication timestamp at chrono's representable edge
    /// plus any age overflows — `DeadlineOverflow`, never a wrap.
    #[test]
    fn retention_from_publication_refuses_a_deadline_overflow() {
        let ttl = ParkTtl::try_new(3600).expect("one hour validates");
        assert_eq!(
            RetentionExpiresAt::from_publication(DateTime::<Utc>::MAX_UTC, ttl),
            Err(RetentionError::DeadlineOverflow),
        );
    }

    /// E5-R R4 guard: a normal stamp round-trips through `as_datetime` as
    /// `published_at + age`.
    #[test]
    fn retention_from_publication_stamps_publication_plus_age() {
        let published = Utc::now();
        let ttl = ParkTtl::try_new(7200).expect("two hours validate");
        let stamped =
            RetentionExpiresAt::from_publication(published, ttl).expect("a normal stamp succeeds");
        let age = chrono::Duration::try_seconds(7200).expect("7200s fits chrono");
        assert_eq!(stamped.as_datetime(), published + age);
    }
}
