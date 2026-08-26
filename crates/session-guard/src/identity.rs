//! Validated identities. These are the only values that may cross the
//! crate boundary where a session, turn, pod, holder, or commit is
//! named — every path derivation and every SQL predicate downstream
//! takes these, never raw strings.
//!
//! Identity roles, one type each:
//!
//! - [`PodId`] names the pod (stable for the pod's lifetime) — the
//!   `holder_pod` column, so a controller can map pod-Deleted to a row.
//! - [`HolderId`] names one acquire *attempt* (fresh UUIDv7 per claim
//!   attempt) — the second fence (invariant I2): heartbeat/commit/release
//!   predicates on it, so a Postgres failover that regresses the epoch
//!   still cannot linearize a stale holder's commit.
//! - [`OpId`] names one *logical* commit — the commit-unknown
//!   reconciliation key (retries reuse it; the read-back distinguishes
//!   Applied from NotApplied).

use std::fmt;

/// Maximum session id length in bytes.
pub const SESSION_ID_MAX_BYTES: usize = 128;
/// Maximum pod id length in bytes (k8s pod names fit RFC 1123's 253).
pub const POD_ID_MAX_BYTES: usize = 253;

const RESERVED_SESSION_IDS: [&str; 3] = [".", "..", "latest"];

/// A session identity that is always a safe single path component.
///
/// Business rule: exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, and
/// not `.` / `..` / `latest` (empty included — `root.join("")` resolves
/// to the session root itself). Case-sensitive verbatim — a typo is a new
/// session, by design (matches today's `X-Chat-Session-Id` matching).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

/// Why a raw string is not a [`SessionId`]. Diagnostic-only payload.
#[derive(Debug, thiserror::Error)]
#[error("invalid session id: {reason}")]
pub struct InvalidSessionId {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

impl SessionId {
    /// Parse and constrain a session id. The sole constructor.
    ///
    /// # Errors
    /// [`InvalidSessionId`] when the charset, length, or reserved-name
    /// rule fails.
    pub fn parse(raw: &str) -> Result<Self, InvalidSessionId> {
        if raw.is_empty() {
            return Err(InvalidSessionId {
                reason: "empty session id".to_string(),
            });
        }
        if raw.len() > SESSION_ID_MAX_BYTES {
            return Err(InvalidSessionId {
                reason: format!("session id exceeds {SESSION_ID_MAX_BYTES} bytes"),
            });
        }
        if RESERVED_SESSION_IDS.contains(&raw) {
            return Err(InvalidSessionId {
                reason: format!("reserved session id {raw:?}"),
            });
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        {
            return Err(InvalidSessionId {
                reason: "session id contains characters outside [A-Za-z0-9._-]".to_string(),
            });
        }
        Ok(Self(raw.to_string()))
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One orchestration turn. Two wire forms, deliberately: the serde form
/// (compact UUID, manifest JSONB — Rust-to-Rust) and [`parse`](Self::parse)
/// (UUID string — HTTP headers, PG uuid text). Locally minted as UUIDv7
/// (time-ordered, matches run-dir naming); both wire forms accept any
/// UUID so rows written by other writers keep parsing. A parked turn is
/// reified with the *same* `TurnId` (the S1 parked predicate compares it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TurnId(#[serde(with = "uuid::serde::compact")] uuid::Uuid);

impl TurnId {
    /// Generate a new turn id.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Parse a turn id from its wire (UUID string) form. Accepts any
    /// valid UUID: rows written by older writers must keep parsing.
    ///
    /// # Errors
    /// [`InvalidTurnId`] when the string is not a UUID.
    pub fn parse(raw: &str) -> Result<Self, InvalidTurnId> {
        let uuid = uuid::Uuid::parse_str(raw).map_err(|e| InvalidTurnId {
            reason: e.to_string(),
        })?;
        Ok(Self(uuid))
    }
}

/// Why a raw string is not a [`TurnId`]. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid turn id: {reason}")]
pub struct InvalidTurnId {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The identity of the pod a holder runs on. Stable for the pod's
/// lifetime: the `holder_pod` column lets a controller map a
/// pod-Deleted/Failed event to the claim row to release (S4's
/// controller variant predicates on it).
///
/// Business rule: 1..=253 bytes of ASCII `[A-Za-z0-9.-]` (RFC 1123
/// subdomain, so a k8s pod name always parses).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PodId(String);

/// Why a raw string is not a [`PodId`]. Diagnostic-only payload.
#[derive(Debug, thiserror::Error)]
#[error("invalid pod id: {reason}")]
pub struct InvalidPodId {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

impl PodId {
    /// Parse and constrain a pod id. The sole string constructor.
    ///
    /// # Errors
    /// [`InvalidPodId`] when the charset or length rule fails.
    pub fn parse(raw: &str) -> Result<Self, InvalidPodId> {
        if raw.is_empty() {
            return Err(InvalidPodId {
                reason: "empty pod id".to_string(),
            });
        }
        if raw.len() > POD_ID_MAX_BYTES {
            return Err(InvalidPodId {
                reason: format!("pod id exceeds {POD_ID_MAX_BYTES} bytes"),
            });
        }
        for seg in raw.split('.') {
            if seg.is_empty() {
                return Err(InvalidPodId {
                    reason: "pod id has an empty segment".to_string(),
                });
            }
            if seg.len() > 63 {
                return Err(InvalidPodId {
                    reason: "pod id segment exceeds 63 characters".to_string(),
                });
            }
            if !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(InvalidPodId {
                    reason: "pod id contains characters outside [A-Za-z0-9.-]".to_string(),
                });
            }
        }
        Ok(Self(raw.to_string()))
    }

    /// Derive this pod's identity: `AURA_POD_ID` (explicit, fail loud
    /// when invalid), else `POD_NAME` (downward API), else `HOSTNAME`.
    ///
    /// # Errors
    /// [`InvalidPodId`] when `AURA_POD_ID` is set but invalid, or no
    /// source yields a valid id.
    pub fn from_env() -> Result<Self, InvalidPodId> {
        Self::from_env_chain(
            std::env::var("AURA_POD_ID").ok().as_deref(),
            std::env::var("POD_NAME").ok().as_deref(),
            std::env::var("HOSTNAME").ok().as_deref(),
        )
    }

    /// Resolve a pod id from the three env sources. The first present
    /// source wins; if it is invalid the error is returned (fail loud,
    /// no fall-through to a later source). No source present is an error:
    /// a pod must not be anonymous.
    pub(crate) fn from_env_chain(
        aura: Option<&str>,
        pod_name: Option<&str>,
        hostname: Option<&str>,
    ) -> Result<Self, InvalidPodId> {
        if let Some(v) = aura {
            return Self::parse(v);
        }
        if let Some(v) = pod_name {
            return Self::parse(v);
        }
        if let Some(v) = hostname {
            return Self::parse(v);
        }
        Err(InvalidPodId {
            reason: "no pod id source: AURA_POD_ID, POD_NAME, and HOSTNAME are all unset"
                .to_string(),
        })
    }
}

impl AsRef<str> for PodId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PodId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One claim acquire *attempt*: minted fresh (UUIDv7) on every S1, never
/// reused. The second fence (invariant I2): every mutation predicates on
/// `(session_id, epoch, holder_id)`, so even if a Postgres failover
/// regresses the epoch, a stale holder's heartbeat/commit/release match
/// zero rows. The wire form accepts any UUID — the invariant is
/// uniqueness, not version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HolderId(uuid::Uuid);

impl HolderId {
    /// Mint a fresh attempt id (adapter-side, at each S1).
    pub(crate) fn mint() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Parse an attempt id from its wire form.
    ///
    /// # Errors
    /// [`InvalidHolderId`] when the string is not a UUID.
    pub fn parse(raw: &str) -> Result<Self, InvalidHolderId> {
        let uuid = uuid::Uuid::parse_str(raw).map_err(|e| InvalidHolderId {
            reason: e.to_string(),
        })?;
        Ok(Self(uuid))
    }
}

/// Why a raw string is not a [`HolderId`]. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid holder id: {reason}")]
pub struct InvalidHolderId {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

impl fmt::Display for HolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One *logical* commit's identity: minted once per turn's barrier,
/// reused by every retry of that commit. Stored in `last_commit_op`; the
/// commit-unknown reconciliation read-back (B3) compares it to tell
/// Applied from NotApplied — never a blind re-UPDATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(uuid::Uuid);

impl OpId {
    /// Mint a fresh logical-commit id (barrier-side, once per barrier).
    pub(crate) fn mint() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Parse an op id from its wire form.
    ///
    /// # Errors
    /// [`InvalidOpId`] when the string is not a UUID.
    pub fn parse(raw: &str) -> Result<Self, InvalidOpId> {
        let uuid = uuid::Uuid::parse_str(raw).map_err(|e| InvalidOpId {
            reason: e.to_string(),
        })?;
        Ok(Self(uuid))
    }
}

/// Why a raw string is not an [`OpId`]. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid op id: {reason}")]
pub struct InvalidOpId {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_present_wins_aura() {
        let pod = PodId::from_env_chain(Some("aura-1"), Some("pod-1"), Some("host-1"))
            .expect("aura source wins");
        assert_eq!(pod.as_ref(), "aura-1");
    }

    #[test]
    fn first_present_wins_pod_name() {
        let pod = PodId::from_env_chain(None, Some("pod-1"), Some("host-1"))
            .expect("pod name source wins");
        assert_eq!(pod.as_ref(), "pod-1");
    }

    #[test]
    fn first_present_wins_hostname() {
        let pod = PodId::from_env_chain(None, None, Some("host-1")).expect("hostname source wins");
        assert_eq!(pod.as_ref(), "host-1");
    }

    #[test]
    fn invalid_first_present_fails_loud() {
        let err = PodId::from_env_chain(Some("bad/name"), Some("pod-1"), Some("host-1"))
            .expect_err("invalid aura source fails loud, no fall-through");
        assert!(!err.reason.is_empty());
    }

    #[test]
    fn none_present_errors() {
        let err = PodId::from_env_chain(None, None, None).expect_err("no source is an error");
        assert!(!err.reason.is_empty());
    }
}
