//! Validated identities. These are the only values that may cross the
//! crate boundary where a session, turn, or holder instance is named —
//! every path derivation downstream takes these, never raw strings.

use std::fmt;

/// Maximum session id length in bytes.
pub const SESSION_ID_MAX_BYTES: usize = 128;
/// Maximum instance id length in bytes.
pub const INSTANCE_ID_MAX_BYTES: usize = 64;

const RESERVED_SESSION_IDS: [&str; 3] = [".", "..", "latest"];

/// A session identity that is always a safe single path component.
///
/// Business rule: exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, and
/// not `.` / `..` / `latest` (empty included — `root.join("")` resolves
/// to the claim root itself). Case-sensitive verbatim — a typo is a new
/// session, by design (matches today's `X-Chat-Session-Id` matching).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

/// Why a raw string is not a [`SessionId`]. Diagnostic-only payload.
#[derive(Debug, thiserror::Error)]
#[error("invalid session id: {reason}")]
pub struct InvalidSessionId {
    pub reason: String,
}

impl SessionId {
    /// Parse and constrain a session id. The sole constructor.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub fn parse(raw: &str) -> Result<Self, InvalidSessionId> {
        todo!("fill: charset/length/reserved rules; aura #421 follow-up")
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

/// One orchestration turn. UUIDv7: time-ordered, matches run-dir naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnId(uuid::Uuid);

impl TurnId {
    /// Generate a new turn id.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
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

/// The identity of the service instance that holds a claim. Deployment
/// neutral: hostname, pod name, VM id, or an explicit operator-chosen id —
/// anything that names one running process for the lifetime of a turn.
/// Written into claims and holder views so routing and steal evidence can
/// name the holder.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceId(String);

/// Why a raw string is not an [`InstanceId`]. Diagnostic-only payload.
#[derive(Debug, thiserror::Error)]
#[error("invalid instance id: {reason}")]
pub struct InvalidInstanceId {
    pub reason: String,
}

impl InstanceId {
    /// Parse and constrain an instance id. The sole string constructor.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub fn parse(raw: &str) -> Result<Self, InvalidInstanceId> {
        todo!("fill: charset/length rules; aura #421 follow-up")
    }

    /// Derive this process's instance id: `AURA_INSTANCE_ID` if set,
    /// else `HOSTNAME` if valid, else a process-unique fallback.
    #[must_use]
    pub fn from_env() -> Self {
        todo!("fill: env/hostname/fallback; aura #421 follow-up")
    }
}

impl AsRef<str> for InstanceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
