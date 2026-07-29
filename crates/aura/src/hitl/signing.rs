#![allow(dead_code)]

//! HMAC-SHA256 root of trust for HITL webhook traffic.
//!
//! Signs AURA's egress approval requests and verifies inbound decisions on
//! both routes. The feature is opt-in via environment variables; with no
//! secret configured the loader yields `None` and callers skip
//! signing/verification entirely, leaving today's byte-identical behavior.
//!
//! Header contract:
//!   X-Aura-Signature-256: sha256=<64 lowercase hex chars>
//!   X-Aura-Timestamp: <unix seconds, canonical decimal>
//!
//! Signed payload: `"{unix_seconds}.{context}.{raw_body_bytes}"`.
//!
//! The `{context}` segment binds each signature to its resource and
//! direction so a captured signature cannot be re-aimed at another resource
//! within the skew window (see `SigningContext` and `DESIGN.md` §1). The
//! timestamp is rendered as canonical decimal and the context forbids the
//! `.` delimiter, so the encoding is injective: two distinct
//! `(timestamp, context, body)` triples cannot collide.
//!
//! Context registry (the exact labels the fill and both seams use):
//!   egress approval-request POST: `approval-request:{decision_id}`
//!   ingress decision POST and webhook-response leg:
//!     `approval-decision:{decision_id}`

use std::fmt;

use bytes::Bytes;

pub const SIGNATURE_HEADER: &str = "X-Aura-Signature-256";
pub const TIMESTAMP_HEADER: &str = "X-Aura-Timestamp";
pub const SIGNATURE_PREFIX: &str = "sha256=";
pub const DEFAULT_TOLERANCE_SECS: u64 = 300;
/// Minimum accepted primary/secondary key length, in bytes (256 bits).
pub const MIN_SECRET_BYTES: usize = 32;
/// Maximum accepted skew tolerance, in seconds (one day).
pub const MAX_TOLERANCE_SECS: u64 = 86_400;

/// Primary HMAC secret. Can both sign egress and verify ingress.
#[derive(Clone)]
pub struct PrimarySecret(SecretBytes);

impl PrimarySecret {
    pub fn new(bytes: &[u8]) -> Self {
        Self(SecretBytes::new(bytes))
    }
}

impl fmt::Debug for PrimarySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrimarySecret").finish_non_exhaustive()
    }
}

/// Secondary HMAC secret. Verifies ingress only; the type has no sign method.
#[derive(Clone)]
pub struct SecondarySecret(SecretBytes);

impl SecondarySecret {
    pub fn new(bytes: &[u8]) -> Self {
        Self(SecretBytes::new(bytes))
    }
}

impl fmt::Debug for SecondarySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondarySecret").finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct SecretBytes(Box<[u8]>);

impl SecretBytes {
    fn new(bytes: &[u8]) -> Self {
        Self(Box::from(bytes))
    }
}

impl AsRef<[u8]> for SecretBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretBytes").finish_non_exhaustive()
    }
}

/// A validated label bound into the signed payload.
///
/// Carries the resource identity and direction (see the module-level context
/// registry). Forbidding the `.` delimiter is what keeps the signed-string
/// encoding injective, so the invariant is enforced at construction rather
/// than assumed by callers.
#[derive(Debug, Clone)]
pub struct SigningContext(String);

impl SigningContext {
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn new(raw: &str) -> Result<Self, ContextError> {
        todo!()
    }
}

impl AsRef<str> for SigningContext {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A parsed `X-Aura-Signature-256` value.
///
/// The raw header string is preserved so the wire form can be replayed
/// without re-serialization; the signature bytes are validated at parse time.
#[derive(Debug, Clone)]
pub struct SignatureHeader {
    raw: String,
    signature: Signature,
}

impl SignatureHeader {
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn parse(value: &str) -> Result<Self, VerificationError> {
        todo!()
    }
}

impl AsRef<str> for SignatureHeader {
    fn as_ref(&self) -> &str {
        &self.raw
    }
}

impl AsRef<Signature> for SignatureHeader {
    fn as_ref(&self) -> &Signature {
        &self.signature
    }
}

/// A 32-byte HMAC-SHA256 signature tag.
///
/// Deliberately has no `PartialEq`/`Eq` and no public byte accessor:
/// comparison happens only inside `WebhookHmac::verify` via a constant-time
/// primitive, so a non-constant-time `==` cannot be written against it.
#[derive(Debug, Clone)]
pub struct Signature([u8; 32]);

/// Unix seconds carried on `X-Aura-Timestamp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixTimestamp(u64);

impl UnixTimestamp {
    /// Parses a canonical decimal timestamp. Rejects a leading `+` and
    /// leading zeros (except the literal `"0"`) so the parsed value renders
    /// back to the exact bytes that were signed.
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn parse(value: &str) -> Result<Self, VerificationError> {
        todo!()
    }

    /// Current unix time. Fallible rather than panicking on a system clock
    /// set before the unix epoch.
    pub fn now() -> Result<Self, ClockError> {
        todo!()
    }
}

impl fmt::Display for UnixTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Configurable skew tolerance for timestamp verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tolerance(u64);

impl Tolerance {
    /// Builds a tolerance, rejecting zero and anything above
    /// `MAX_TOLERANCE_SECS`.
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn new(secs: u64) -> Result<Self, ConfigError> {
        todo!()
    }
}

impl Default for Tolerance {
    fn default() -> Self {
        Self(DEFAULT_TOLERANCE_SECS)
    }
}

impl fmt::Display for Tolerance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The headers produced by signing an egress request.
///
/// The signature and timestamp are a matched pair from one signing operation.
/// Consuming the pair via `into_pairs` (with no field accessors) makes mixing
/// a signature and timestamp from different results deliberate work on the
/// returned strings rather than a zero-effort default; it is atomic-use
/// hygiene, not an unrepresentable state. A mismatched pair is a
/// self-inflicted 401 at the receiver, with no attacker leverage.
#[derive(Debug, Clone)]
pub struct SignedHeaders {
    signature: SignatureHeader,
    timestamp: UnixTimestamp,
}

impl SignedHeaders {
    /// Consumes the pair into `(header_name, header_value)` entries ready to
    /// attach to a request: the `X-Aura-Signature-256` and `X-Aura-Timestamp`
    /// headers, in that order.
    pub fn into_pairs(self) -> [(&'static str, String); 2] {
        todo!()
    }
}

/// Loaded HMAC configuration. `None` from the loader means the feature is off.
///
/// The off state is unrepresentable in `sign`/`verify`: callers must hold a
/// `WebhookHmac` to reach those methods.
#[derive(Clone)]
pub struct WebhookHmac {
    primary: PrimarySecret,
    secondary: Option<SecondarySecret>,
    tolerance: Tolerance,
}

impl WebhookHmac {
    /// Builds a configuration directly. Rejects a primary shorter than
    /// `MIN_SECRET_BYTES`; a shorter secondary is likewise rejected. The
    /// secret-length floor lives here (not on the secret constructors) so the
    /// policy has one home and secret construction stays infallible.
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn new(
        primary: PrimarySecret,
        secondary: Option<SecondarySecret>,
        tolerance: Tolerance,
    ) -> Result<Self, ConfigError> {
        todo!()
    }

    /// Reads `AURA_HITL_WEBHOOK_SECRET`, `AURA_HITL_WEBHOOK_SECRET_SECONDARY`,
    /// and `AURA_HITL_WEBHOOK_TOLERANCE_SECS`.
    ///
    /// Returns `Ok(None)` only when the primary is genuinely absent (feature
    /// off). A secondary without a primary, an empty or too-short primary, a
    /// malformed or out-of-range tolerance, or a non-Unicode value is a
    /// misconfiguration and returns `Err`, so a typo fails loud instead of
    /// silently disabling the control. A thin HITL-named wrapper over `new`.
    pub fn load_from_env() -> Result<Option<Self>, ConfigError> {
        todo!()
    }

    #[must_use]
    pub fn tolerance(&self) -> Tolerance {
        self.tolerance
    }

    /// Signs `body` under `context`. Fallible only on a clock error.
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn sign(
        &self,
        context: &SigningContext,
        body: &[u8],
    ) -> Result<SignedHeaders, SigningError> {
        todo!()
    }

    /// Verifies a parsed signature over `"{timestamp}.{context}.{body}"`,
    /// checking skew and comparing in constant time. Tries the primary, then
    /// the secondary if configured; both candidates are evaluated on any
    /// non-primary-match path so the fallback reveals nothing beyond "did the
    /// primary sign this" to a party already holding a key.
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub(crate) fn verify(
        &self,
        context: &SigningContext,
        signature: &SignatureHeader,
        timestamp: UnixTimestamp,
        body: &[u8],
    ) -> Result<(), VerificationError> {
        todo!()
    }
}

impl fmt::Debug for WebhookHmac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookHmac")
            .field("primary", &self.primary)
            .field("secondary", &self.secondary)
            .field("tolerance", &self.tolerance)
            .finish_non_exhaustive()
    }
}

/// Error building a [`SigningContext`].
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("signing context must not be empty")]
    Empty,
    #[error("signing context must not contain the '.' delimiter")]
    ContainsDelimiter,
    #[error("signing context must be ASCII")]
    NonAscii,
}

/// Error loading or building a [`WebhookHmac`] configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("primary secret is shorter than the {MIN_SECRET_BYTES}-byte minimum")]
    PrimaryTooShort { len: usize },
    #[error("secondary secret is shorter than the {MIN_SECRET_BYTES}-byte minimum")]
    SecondaryTooShort { len: usize },
    #[error("a secondary secret is configured without a primary")]
    SecondaryWithoutPrimary,
    #[error("tolerance is not a valid integer number of seconds")]
    MalformedTolerance,
    #[error("tolerance {secs}s is outside 1..={MAX_TOLERANCE_SECS}")]
    ToleranceOutOfRange { secs: u64 },
    #[error("environment variable {var} is not valid Unicode")]
    NonUnicodeValue { var: &'static str },
}

/// System clock unavailable (set before the unix epoch).
#[derive(Debug, thiserror::Error)]
#[error("system clock is set before the unix epoch")]
pub struct ClockError;

/// Error signing an egress request.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error(transparent)]
    Clock(#[from] ClockError),
}

/// Errors that can occur when verifying an inbound request.
///
/// Every variant maps to a single uniform `401` on the wire; the variant is
/// for logs only. In particular `SkewedTimestamp`'s `Display` (which names
/// `now`/`tolerance`) must never reach a response body (see `DESIGN.md` §2).
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("missing signature header")]
    MissingSignatureHeader,
    #[error("missing timestamp header")]
    MissingTimestampHeader,
    #[error("malformed signature header")]
    MalformedSignature,
    #[error("malformed timestamp header")]
    MalformedTimestamp,
    #[error("timestamp skewed: provided={provided}, now={now}, tolerance={tolerance}")]
    SkewedTimestamp {
        provided: UnixTimestamp,
        now: UnixTimestamp,
        tolerance: Tolerance,
    },
    #[error("signature mismatch")]
    Mismatch,
    #[error(transparent)]
    Clock(#[from] ClockError),
}

/// Body that has passed ingress authorization.
///
/// Wraps the immutable `Bytes` it was verified over. It has no public
/// constructor other than [`authorize_ingress`], so possessing one proves
/// authorization ran on exactly these bytes. The witness holds `Bytes`
/// specifically — not a generic `AsRef<[u8]>` — so that the verified bytes
/// are frozen at authorization time and a later read cannot return a
/// different slice than the one that was verified.
pub struct VerifiedBody(Bytes);

impl VerifiedBody {
    pub fn into_inner(self) -> Bytes {
        self.0
    }
}

impl AsRef<[u8]> for VerifiedBody {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Authorizes an inbound request under `context`.
///
/// - If `config` is `None`, the feature is off and `body` is returned
///   unverified.
/// - If `config` is `Some`, an absent signature or timestamp header maps to
///   `MissingSignatureHeader` / `MissingTimestampHeader`, then the signature
///   header is parsed, the timestamp is skew-checked, and the signature is
///   verified in constant time over `"{timestamp}.{context}.{body}"`.
///
/// Takes an immutable `Bytes` so the verified content is frozen and the only
/// way past this function with the bytes in hand is through the returned
/// [`VerifiedBody`]. Used by both the ingress decision handler (axum's `Bytes`
/// extractor) and the webhook-response leg (reqwest's response `Bytes`) — same
/// primitive, different context label.
#[expect(unused_variables, reason = "todo!() body; filled by W3")]
pub fn authorize_ingress(
    config: Option<&WebhookHmac>,
    context: &SigningContext,
    signature_header: Option<&str>,
    timestamp_header: Option<&str>,
    body: Bytes,
) -> Result<VerifiedBody, VerificationError> {
    todo!()
}
