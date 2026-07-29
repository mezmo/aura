#![allow(dead_code)]

//! HMAC-SHA256 root of trust for HITL webhook traffic.
//!
//! Signs AURA's egress approval requests and verifies ingress decision
//! posts. The feature is opt-in via environment variables; with no secret
//! configured the module loads `None` and callers skip signing/verification
//! entirely, leaving today's byte-identical behavior.
//!
//! Header contract:
//!   X-Aura-Signature-256: sha256=<64 lowercase hex chars>
//!   X-Aura-Timestamp: <unix seconds>
//!
//! Signed payload: "{unix_seconds}.{raw_body_bytes}".

use std::fmt;

pub const SIGNATURE_HEADER: &str = "X-Aura-Signature-256";
pub const TIMESTAMP_HEADER: &str = "X-Aura-Timestamp";
pub const SIGNATURE_PREFIX: &str = "sha256=";
pub const DEFAULT_TOLERANCE_SECS: u64 = 300;

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
#[derive(Debug, Clone)]
pub struct Signature([u8; 32]);

/// Unix seconds carried on `X-Aura-Timestamp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixTimestamp(u64);

impl UnixTimestamp {
    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn parse(value: &str) -> Result<Self, VerificationError> {
        todo!()
    }

    pub fn now() -> Self {
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
    pub const fn new(secs: u64) -> Self {
        Self(secs)
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
/// Both fields are validated: the signature matches the body and timestamp,
/// and the timestamp is the one used in the signed payload.
#[derive(Debug, Clone)]
pub struct SignedHeaders {
    signature: SignatureHeader,
    timestamp: UnixTimestamp,
}

impl SignedHeaders {
    pub fn signature(&self) -> &SignatureHeader {
        &self.signature
    }

    pub fn timestamp(&self) -> &UnixTimestamp {
        &self.timestamp
    }
}

/// Loaded HMAC configuration. `None` means the feature is off.
///
/// The off state is unrepresentable in `sign`/`verify`: callers must hold
/// a `WebhookHmac` to call those methods.
#[derive(Clone)]
pub struct WebhookHmac {
    primary: PrimarySecret,
    secondary: Option<SecondarySecret>,
    tolerance: Tolerance,
}

impl WebhookHmac {
    pub fn load_from_env() -> Option<Self> {
        todo!()
    }

    #[must_use]
    pub fn tolerance(&self) -> Tolerance {
        self.tolerance
    }

    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub fn sign(&self, body: &[u8]) -> SignedHeaders {
        todo!()
    }

    #[expect(unused_variables, reason = "todo!() body; filled by W3")]
    pub(crate) fn verify(
        &self,
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

/// Errors that can occur when verifying an ingress request.
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
}

/// Body that has passed ingress authorization.
///
/// When webhook verification is configured, this is only produced after the
/// signature and timestamp headers are present, the timestamp is within the
/// configured skew tolerance, and the signature matches the recomputed HMAC.
/// When verification is disabled (`config` is `None`), the body passes through
/// unverified.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedBody<'a>(&'a [u8]);

impl<'a> AsRef<[u8]> for VerifiedBody<'a> {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

/// Authorizes an ingress request.
///
/// - If `config` is `None`, the feature is off and the body is returned
///   unverified.
/// - If `config` is `Some`, missing headers produce
///   `VerificationError::MissingSignatureHeader` or
///   `VerificationError::MissingTimestampHeader`, then the signature header is
///   parsed, the timestamp is checked for skew, and the signature is verified
///   in constant time.
#[expect(unused_variables, reason = "todo!() body; filled by W3")]
pub fn authorize_ingress<'a>(
    config: Option<&WebhookHmac>,
    signature_header: Option<&str>,
    timestamp_header: Option<&str>,
    body: &'a [u8],
) -> Result<VerifiedBody<'a>, VerificationError> {
    todo!()
}
