//! The validated absolute retention deadline carried on park event
//! surfaces.
//!
//! Retention is disk-evidence lifetime. Every surface that names a
//! retention deadline on the wire — `RunParked` events here and the
//! checkpoint's persisted field in the `aura` crate — carries a value that
//! is an RFC 3339 instant by construction: a stamp like
//! `"not-an-instant"` fails the fallible constructor and can never ride an
//! event. The wire form stays the RFC 3339 string.

use serde::{Deserialize, Serialize};

/// Why a retention deadline stamp failed validation. Carries the rejected
/// wire value so a decode failure can name it; no caller branches on the
/// contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRetentionStamp {
    /// The rejected wire value.
    pub stamp: String,
}

impl std::fmt::Display for InvalidRetentionStamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "retention deadline is not an RFC 3339 instant: {:?}",
            self.stamp
        )
    }
}

impl std::error::Error for InvalidRetentionStamp {}

/// An absolute retention deadline: the instant after which a run's parked
/// evidence may be reclaimed, as an RFC 3339 instant by construction.
///
/// The wire form is the RFC 3339 string (chrono's canonical `Z`-suffixed
/// rendering for UTC); decoding runs through the same validation, so an
/// undecodable instant never crosses this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RetentionExpiresAt(chrono::DateTime<chrono::Utc>);

impl RetentionExpiresAt {
    /// Wrap an already-resolved absolute deadline.
    #[must_use]
    pub fn from_datetime(deadline: chrono::DateTime<chrono::Utc>) -> Self {
        Self(deadline)
    }

    /// Parse an RFC 3339 wire stamp into the validated deadline.
    pub fn parse_rfc3339(stamp: &str) -> Result<Self, InvalidRetentionStamp> {
        chrono::DateTime::parse_from_rfc3339(stamp)
            .map(|parsed| Self(parsed.with_timezone(&chrono::Utc)))
            .map_err(|_| InvalidRetentionStamp {
                stamp: stamp.to_string(),
            })
    }

    /// The absolute deadline.
    #[must_use]
    pub fn as_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        self.0
    }

    /// The RFC 3339 wire rendering.
    #[must_use]
    pub fn to_rfc3339(&self) -> String {
        self.0.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
    }
}

impl TryFrom<String> for RetentionExpiresAt {
    type Error = InvalidRetentionStamp;

    fn try_from(stamp: String) -> Result<Self, Self::Error> {
        Self::parse_rfc3339(&stamp)
    }
}

impl From<RetentionExpiresAt> for String {
    fn from(deadline: RetentionExpiresAt) -> Self {
        deadline.to_rfc3339()
    }
}

impl std::fmt::Display for RetentionExpiresAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip preserves the instant and the canonical `Z` wire form.
    #[test]
    fn rfc3339_round_trip() {
        let deadline = RetentionExpiresAt::parse_rfc3339("2026-09-02T15:03:11Z")
            .expect("a canonical UTC stamp parses");
        assert_eq!(deadline.to_rfc3339(), "2026-09-02T15:03:11Z");

        let json = serde_json::to_string(&deadline).unwrap();
        assert_eq!(json, "\"2026-09-02T15:03:11Z\"");
        assert_eq!(
            serde_json::from_str::<RetentionExpiresAt>(&json).unwrap(),
            deadline
        );
    }

    /// An offset form parses and re-renders in the canonical UTC form; a
    /// non-instant is refused by construction and by decode.
    #[test]
    fn non_instants_are_unrepresentable() {
        let deadline = RetentionExpiresAt::parse_rfc3339("2026-09-02T15:03:11+00:00")
            .expect("an offset RFC 3339 stamp parses");
        assert_eq!(deadline.to_rfc3339(), "2026-09-02T15:03:11Z");

        assert!(RetentionExpiresAt::parse_rfc3339("not-an-instant").is_err());
        assert!(serde_json::from_str::<RetentionExpiresAt>("\"not-an-instant\"").is_err());
        assert!(
            serde_json::from_str::<RetentionExpiresAt>("\"2026-13-45T99:00:00Z\"").is_err(),
            "an impossible date-time is not an instant"
        );
    }
}
