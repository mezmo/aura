//! HMAC-SHA256 root of trust for HITL webhook traffic.
//!
//! Signs AURA's egress approval requests and verifies inbound decisions on
//! both routes. The feature is opt-in via environment variables; with no
//! secret configured the loader yields `None` and callers skip
//! signing/verification entirely.
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
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tracing::{info, warn};

type HmacSha256 = Hmac<Sha256>;

const PRIMARY_SECRET_VAR: &str = "AURA_HITL_WEBHOOK_SECRET";
const SECONDARY_SECRET_VAR: &str = "AURA_HITL_WEBHOOK_SECRET_SECONDARY";
const TOLERANCE_VAR: &str = "AURA_HITL_WEBHOOK_TOLERANCE_SECS";

pub const SIGNATURE_HEADER: &str = "X-Aura-Signature-256";
pub const TIMESTAMP_HEADER: &str = "X-Aura-Timestamp";
pub const SIGNATURE_PREFIX: &str = "sha256=";
pub const DEFAULT_TOLERANCE_SECS: u64 = 300;
/// Minimum accepted primary/secondary key length, in bytes (256 bits).
pub const MIN_SECRET_BYTES: usize = 32;
/// Maximum accepted skew tolerance, in seconds (one day).
pub const MAX_TOLERANCE_SECS: u64 = 86_400;

/// Primary HMAC secret.
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

/// Secondary HMAC secret. The type has no sign method.
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
    pub fn new(raw: &str) -> Result<Self, ContextError> {
        if raw.is_empty() {
            return Err(ContextError::Empty);
        }
        if !raw.is_ascii() {
            return Err(ContextError::NonAscii);
        }
        if raw.contains('.') {
            return Err(ContextError::ContainsDelimiter);
        }
        Ok(Self(raw.to_string()))
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
    pub fn parse(value: &str) -> Result<Self, VerificationError> {
        let hex_part = value
            .strip_prefix(SIGNATURE_PREFIX)
            .ok_or(VerificationError::MalformedSignature)?;
        if hex_part.len() != 64
            || !hex_part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(VerificationError::MalformedSignature);
        }
        let mut tag = [0u8; 32];
        hex::decode_to_slice(hex_part, &mut tag)
            .map_err(|_| VerificationError::MalformedSignature)?;
        Ok(Self {
            raw: value.to_string(),
            signature: Signature(tag),
        })
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
/// comparison is confined to the constant-time primitive inside this
/// module, so a non-constant-time `==` cannot be written against it.
#[derive(Debug, Clone)]
pub struct Signature([u8; 32]);

/// Unix seconds carried on `X-Aura-Timestamp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixTimestamp(u64);

impl UnixTimestamp {
    /// Parses a canonical decimal timestamp. Rejects a leading `+` and
    /// leading zeros (except the literal `"0"`) so the parsed value renders
    /// back to the exact bytes that were signed.
    pub fn parse(value: &str) -> Result<Self, VerificationError> {
        if value.is_empty()
            || !value.bytes().all(|b| b.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(VerificationError::MalformedTimestamp);
        }
        value
            .parse::<u64>()
            .map(Self)
            .map_err(|_| VerificationError::MalformedTimestamp)
    }

    /// Current unix time. Fallible rather than panicking on a system clock
    /// set before the unix epoch.
    pub fn now() -> Result<Self, ClockError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| Self(elapsed.as_secs()))
            .map_err(|_| ClockError)
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
    pub fn new(secs: u64) -> Result<Self, ConfigError> {
        if secs == 0 || secs > MAX_TOLERANCE_SECS {
            return Err(ConfigError::ToleranceOutOfRange { secs });
        }
        Ok(Self(secs))
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
        [
            (SIGNATURE_HEADER, self.signature.raw),
            (TIMESTAMP_HEADER, self.timestamp.to_string()),
        ]
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
    pub fn new(
        primary: PrimarySecret,
        secondary: Option<SecondarySecret>,
        tolerance: Tolerance,
    ) -> Result<Self, ConfigError> {
        let primary_len = primary.0.0.len();
        if primary_len < MIN_SECRET_BYTES {
            return Err(ConfigError::PrimaryTooShort { len: primary_len });
        }
        if let Some(secondary) = &secondary {
            let secondary_len = secondary.0.0.len();
            if secondary_len < MIN_SECRET_BYTES {
                return Err(ConfigError::SecondaryTooShort { len: secondary_len });
            }
        }
        Ok(Self {
            primary,
            secondary,
            tolerance,
        })
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
        let primary = read_env(PRIMARY_SECRET_VAR)?;
        let secondary = read_env(SECONDARY_SECRET_VAR)?;
        let tolerance = read_env(TOLERANCE_VAR)?;

        let Some(primary) = primary else {
            if secondary.is_some() {
                return Err(ConfigError::SecondaryWithoutPrimary);
            }
            warn!(
                "HITL webhook HMAC verification DISABLED: {PRIMARY_SECRET_VAR} is not set; \
                 webhook traffic is unsigned and unverified"
            );
            return Ok(None);
        };
        // Present-but-empty (or whitespace-only) is a misconfiguration, not a
        // disable: a silently disabled security control is the failure mode
        // the security vet flagged (DESIGN.md residual risk 2).
        if primary.trim().is_empty() {
            return Err(ConfigError::PrimaryTooShort { len: 0 });
        }
        if let Some(secondary) = &secondary
            && secondary.trim().is_empty()
        {
            return Err(ConfigError::SecondaryTooShort { len: 0 });
        }

        let tolerance = match tolerance {
            None => Tolerance::default(),
            Some(raw) => {
                let secs = raw
                    .parse::<u64>()
                    .map_err(|_| ConfigError::MalformedTolerance)?;
                Tolerance::new(secs)?
            }
        };

        let config = Self::new(
            PrimarySecret::new(primary.as_bytes()),
            secondary.map(|s| SecondarySecret::new(s.as_bytes())),
            tolerance,
        )?;
        info!(
            secondary_configured = config.secondary.is_some(),
            tolerance_secs = config.tolerance.0,
            "HITL webhook HMAC signing and verification enabled"
        );
        Ok(Some(config))
    }

    #[must_use]
    pub fn tolerance(&self) -> Tolerance {
        self.tolerance
    }

    /// Signs `body` under `context`. Fallible only on a clock error.
    pub fn sign(
        &self,
        context: &SigningContext,
        body: &[u8],
    ) -> Result<SignedHeaders, SigningError> {
        let timestamp = UnixTimestamp::now()?;
        let tag: [u8; 32] = keyed_mac(&self.primary.0, timestamp, context, body)
            .finalize()
            .into_bytes()
            .into();
        let raw = format!("{SIGNATURE_PREFIX}{}", hex::encode(tag));
        Ok(SignedHeaders {
            signature: SignatureHeader {
                raw,
                signature: Signature(tag),
            },
            timestamp,
        })
    }

    /// Verifies a parsed signature over `"{timestamp}.{context}.{body}"`,
    /// checking skew and comparing in constant time. Tries the primary, then
    /// the secondary if configured; both candidates are evaluated on any
    /// non-primary-match path so the fallback reveals nothing beyond "did the
    /// primary sign this" to a party already holding a key.
    pub(crate) fn verify(
        &self,
        context: &SigningContext,
        signature: &SignatureHeader,
        timestamp: UnixTimestamp,
        body: &[u8],
    ) -> Result<(), VerificationError> {
        let now = UnixTimestamp::now()?;
        if now.0.abs_diff(timestamp.0) > self.tolerance.0 {
            return Err(VerificationError::SkewedTimestamp {
                provided: timestamp,
                now,
                tolerance: self.tolerance,
            });
        }
        // Both candidates are computed before either result is inspected, so
        // the fallback never varies timing or the error variant by which
        // secret failed (DESIGN.md residual risk 3).
        let primary_ok = keyed_mac(&self.primary.0, timestamp, context, body)
            .verify_slice(&signature.signature.0)
            .is_ok();
        let secondary_ok = self.secondary.as_ref().is_some_and(|secondary| {
            keyed_mac(&secondary.0, timestamp, context, body)
                .verify_slice(&signature.signature.0)
                .is_ok()
        });
        if primary_ok || secondary_ok {
            Ok(())
        } else {
            Err(VerificationError::Mismatch)
        }
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
pub fn authorize_ingress(
    config: Option<&WebhookHmac>,
    context: &SigningContext,
    signature_header: Option<&str>,
    timestamp_header: Option<&str>,
    body: Bytes,
) -> Result<VerifiedBody, VerificationError> {
    let Some(config) = config else {
        return Ok(VerifiedBody(body));
    };
    let signature_header = signature_header.ok_or(VerificationError::MissingSignatureHeader)?;
    let timestamp_header = timestamp_header.ok_or(VerificationError::MissingTimestampHeader)?;
    let signature = SignatureHeader::parse(signature_header)?;
    let timestamp = UnixTimestamp::parse(timestamp_header)?;
    config.verify(context, &signature, timestamp, &body)?;
    Ok(VerifiedBody(body))
}

/// One HMAC keyed on `secret` over the canonical signed payload
/// `"{timestamp}.{context}.{body}"`. Shared by `sign` and `verify` so the two
/// legs cannot drift on the payload encoding.
fn keyed_mac(
    secret: &SecretBytes,
    timestamp: UnixTimestamp,
    context: &SigningContext,
    body: &[u8],
) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret.as_ref())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(context.as_ref().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// Reads one env var, distinguishing unset (`Ok(None)`) from a value that is
/// not valid Unicode (`Err`), which is a misconfiguration rather than "off".
fn read_env(var: &'static str) -> Result<Option<String>, ConfigError> {
    match std::env::var(var) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::NonUnicodeValue { var }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// 32-byte primary used across tests (also the known-answer vector key).
    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    const OTHER_KEY: &[u8] = b"fedcba9876543210fedcba9876543210";

    fn hmac_with(primary: &[u8], secondary: Option<&[u8]>, tolerance_secs: u64) -> WebhookHmac {
        WebhookHmac::new(
            PrimarySecret::new(primary),
            secondary.map(SecondarySecret::new),
            Tolerance::new(tolerance_secs).unwrap(),
        )
        .unwrap()
    }

    fn context(label: &str) -> SigningContext {
        SigningContext::new(label).unwrap()
    }

    /// Builds a valid signature header for an arbitrary timestamp, so skew
    /// cases can present a correctly signed but stale/future timestamp.
    fn header_for(key: &[u8], ts: UnixTimestamp, ctx: &SigningContext, body: &[u8]) -> String {
        let tag: [u8; 32] = keyed_mac(&SecretBytes::new(key), ts, ctx, body)
            .finalize()
            .into_bytes()
            .into();
        format!("{SIGNATURE_PREFIX}{}", hex::encode(tag))
    }

    // --- known-answer vector ---

    #[test]
    fn known_answer_hmac_vector() {
        // Externally computed:
        //   python3 -c 'import hmac, hashlib;
        //     print(hmac.new(b"0123456789abcdef0123456789abcdef",
        //       b"1700000000.approval-request:018f2c1a-7b3d-7000-8000-000000000000"
        //       b".{\"approved\":true}", hashlib.sha256).hexdigest())'
        let ctx = context("approval-request:018f2c1a-7b3d-7000-8000-000000000000");
        let ts = UnixTimestamp::parse("1700000000").unwrap();
        let tag: [u8; 32] = keyed_mac(&SecretBytes::new(KEY), ts, &ctx, b"{\"approved\":true}")
            .finalize()
            .into_bytes()
            .into();
        assert_eq!(
            hex::encode(tag),
            "4580ab4e23773678507ac8db54a4400bd8e6576e9057de4be698ee5b831aae66"
        );
    }

    // --- sign / authorize round trips ---

    #[test]
    fn sign_then_authorize_accepts() {
        let config = hmac_with(KEY, None, 300);
        let ctx = context("approval-decision:d1");
        let body = Bytes::from_static(b"{\"approved\":true}");
        let [(sig_name, sig), (ts_name, ts)] = config.sign(&ctx, &body).unwrap().into_pairs();
        assert_eq!(sig_name, SIGNATURE_HEADER);
        assert_eq!(ts_name, TIMESTAMP_HEADER);
        assert!(sig.starts_with(SIGNATURE_PREFIX));

        let verified =
            authorize_ingress(Some(&config), &ctx, Some(&sig), Some(&ts), body.clone()).unwrap();
        assert_eq!(verified.as_ref(), body.as_ref());
    }

    #[test]
    fn tampered_body_rejected() {
        let config = hmac_with(KEY, None, 300);
        let ctx = context("approval-decision:d1");
        let [(_, sig), (_, ts)] = config
            .sign(&ctx, b"{\"approved\":true}")
            .unwrap()
            .into_pairs();

        let tampered = Bytes::from_static(b"{\"approved\":false}");
        let err = authorize_ingress(Some(&config), &ctx, Some(&sig), Some(&ts), tampered)
            .err()
            .expect("expected a verification rejection");
        assert!(matches!(err, VerificationError::Mismatch));
    }

    #[test]
    fn cross_context_rejected() {
        let config = hmac_with(KEY, None, 300);
        let signed_ctx = context("approval-decision:d1");
        let other_ctx = context("approval-decision:d2");
        let body = Bytes::from_static(b"{\"approved\":true}");
        let [(_, sig), (_, ts)] = config.sign(&signed_ctx, &body).unwrap().into_pairs();

        let err = authorize_ingress(Some(&config), &other_ctx, Some(&sig), Some(&ts), body)
            .err()
            .expect("expected a verification rejection");
        assert!(matches!(err, VerificationError::Mismatch));
    }

    // --- skew ---

    #[test]
    fn skew_rejected_in_both_directions() {
        let config = hmac_with(KEY, None, 300);
        let ctx = context("approval-decision:d1");
        let body = Bytes::from_static(b"x");
        let now = UnixTimestamp::now().unwrap();

        for stale in [now.0 - 400, now.0 + 400] {
            let ts = UnixTimestamp(stale);
            let sig = header_for(KEY, ts, &ctx, &body);
            let err = authorize_ingress(
                Some(&config),
                &ctx,
                Some(&sig),
                Some(&ts.to_string()),
                body.clone(),
            )
            .err()
            .expect("expected a verification rejection");
            assert!(
                matches!(err, VerificationError::SkewedTimestamp { .. }),
                "offset {stale} should be rejected as skewed, got {err:?}"
            );
        }
    }

    #[test]
    fn skew_within_tolerance_accepted_in_both_directions() {
        let config = hmac_with(KEY, None, 300);
        let ctx = context("approval-decision:d1");
        let body = Bytes::from_static(b"x");
        let now = UnixTimestamp::now().unwrap();

        // 250s offsets leave 50s of margin for wall-clock ticks mid-test.
        for near in [now.0 - 250, now.0 + 250] {
            let ts = UnixTimestamp(near);
            let sig = header_for(KEY, ts, &ctx, &body);
            authorize_ingress(
                Some(&config),
                &ctx,
                Some(&sig),
                Some(&ts.to_string()),
                body.clone(),
            )
            .unwrap_or_else(|err| panic!("offset {near} should verify, got {err:?}"));
        }
    }

    #[test]
    fn skew_extremes_do_not_overflow() {
        let config = hmac_with(KEY, None, 300);
        let ctx = context("approval-decision:d1");
        let body = Bytes::from_static(b"x");

        for extreme in [0u64, u64::MAX] {
            let ts = UnixTimestamp(extreme);
            let sig = header_for(KEY, ts, &ctx, &body);
            let err = authorize_ingress(
                Some(&config),
                &ctx,
                Some(&sig),
                Some(&ts.to_string()),
                body.clone(),
            )
            .err()
            .expect("expected a verification rejection");
            assert!(matches!(err, VerificationError::SkewedTimestamp { .. }));
        }
    }

    // --- rotation ---

    #[test]
    fn secondary_secret_rotation_accepts_old_signatures() {
        let old = hmac_with(KEY, None, 300);
        let rotated = hmac_with(OTHER_KEY, Some(KEY), 300);
        let ctx = context("approval-decision:d1");
        let body = Bytes::from_static(b"{\"approved\":true}");
        let [(_, sig), (_, ts)] = old.sign(&ctx, &body).unwrap().into_pairs();

        // Signed under the old primary; the rotated config verifies it via
        // its secondary.
        authorize_ingress(Some(&rotated), &ctx, Some(&sig), Some(&ts), body.clone())
            .expect("rotated config must accept old-primary signatures");

        // Without the secondary, the same signature is a mismatch.
        let new_only = hmac_with(OTHER_KEY, None, 300);
        let err = authorize_ingress(Some(&new_only), &ctx, Some(&sig), Some(&ts), body)
            .err()
            .expect("expected a verification rejection");
        assert!(matches!(err, VerificationError::Mismatch));
    }

    // --- missing headers / feature off ---

    #[test]
    fn missing_headers_rejected_when_secret_configured() {
        let config = hmac_with(KEY, None, 300);
        let ctx = context("approval-decision:d1");
        let body = Bytes::from_static(b"x");

        let err = authorize_ingress(Some(&config), &ctx, None, Some("1700000000"), body.clone())
            .err()
            .expect("expected a verification rejection");
        assert!(matches!(err, VerificationError::MissingSignatureHeader));

        let sig = header_for(KEY, UnixTimestamp(1_700_000_000), &ctx, &body);
        let err = authorize_ingress(Some(&config), &ctx, Some(&sig), None, body)
            .err()
            .expect("expected a verification rejection");
        assert!(matches!(err, VerificationError::MissingTimestampHeader));
    }

    #[test]
    fn feature_off_passes_everything_through() {
        let ctx = context("approval-decision:d1");
        let body = Bytes::from_static(b"{\"approved\":true}");
        for (sig, ts) in [
            (None, None),
            (Some("garbage"), Some("also-garbage")),
            (Some("sha256=00"), Some("+123")),
        ] {
            let verified = authorize_ingress(None, &ctx, sig, ts, body.clone())
                .expect("no configured secret must mean no verification");
            assert_eq!(verified.as_ref(), body.as_ref());
        }
    }

    // --- parsing ---

    #[test]
    fn signature_header_parse_accepts_canonical_form() {
        let value = format!("sha256={}", "ab".repeat(32));
        let parsed = SignatureHeader::parse(&value).unwrap();
        assert_eq!(parsed.as_ref() as &str, value);
    }

    #[test]
    fn signature_header_parse_rejects_malformed() {
        let valid_hex = "ab".repeat(32);
        for bad in [
            "",
            "deadbeef",
            &format!("sha1={valid_hex}"),
            &format!("sha256={}", "AB".repeat(32)), // uppercase hex
            &format!("sha256={}", "ab".repeat(31)), // too short
            &format!("sha256={}", "ab".repeat(33)), // too long
            &format!("sha256={}", "zz".repeat(32)), // non-hex
            &format!(" sha256={valid_hex}"),
        ] {
            let err = SignatureHeader::parse(bad).unwrap_err();
            assert!(
                matches!(err, VerificationError::MalformedSignature),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn timestamp_parse_is_canonical_only() {
        assert_eq!(UnixTimestamp::parse("0").unwrap(), UnixTimestamp(0));
        assert_eq!(
            UnixTimestamp::parse("1700000000").unwrap(),
            UnixTimestamp(1_700_000_000)
        );
        for bad in [
            "",
            "+1700000000",
            "-5",
            "01",
            "007",
            "1.7e9",
            "1700000000 ",
            "abc",
            "99999999999999999999999", // overflows u64
        ] {
            let err = UnixTimestamp::parse(bad).unwrap_err();
            assert!(
                matches!(err, VerificationError::MalformedTimestamp),
                "{bad:?} must be rejected"
            );
        }
        // A rendered timestamp is canonical, so it parses back unchanged.
        let now = UnixTimestamp::now().unwrap();
        assert_eq!(UnixTimestamp::parse(&now.to_string()).unwrap(), now);
    }

    // --- constrained construction ---

    #[test]
    fn signing_context_validation() {
        assert!(SigningContext::new("approval-decision:d1").is_ok());
        assert!(matches!(
            SigningContext::new("").unwrap_err(),
            ContextError::Empty
        ));
        assert!(matches!(
            SigningContext::new("a.b").unwrap_err(),
            ContextError::ContainsDelimiter
        ));
        assert!(matches!(
            SigningContext::new("café").unwrap_err(),
            ContextError::NonAscii
        ));
    }

    #[test]
    fn tolerance_bounds() {
        assert!(Tolerance::new(1).is_ok());
        assert!(Tolerance::new(MAX_TOLERANCE_SECS).is_ok());
        assert_eq!(Tolerance::default(), Tolerance(DEFAULT_TOLERANCE_SECS));
        for bad in [0, MAX_TOLERANCE_SECS + 1] {
            assert!(matches!(
                Tolerance::new(bad).unwrap_err(),
                ConfigError::ToleranceOutOfRange { secs } if secs == bad
            ));
        }
    }

    #[test]
    fn secret_length_floor_enforced_in_constructor() {
        let short = b"too-short";
        assert!(matches!(
            WebhookHmac::new(
                PrimarySecret::new(short),
                None,
                Tolerance::default()
            )
            .unwrap_err(),
            ConfigError::PrimaryTooShort { len } if len == short.len()
        ));
        assert!(matches!(
            WebhookHmac::new(
                PrimarySecret::new(KEY),
                Some(SecondarySecret::new(short)),
                Tolerance::default()
            )
            .unwrap_err(),
            ConfigError::SecondaryTooShort { len } if len == short.len()
        ));
    }

    #[test]
    fn debug_output_never_contains_key_material() {
        let config = hmac_with(KEY, Some(OTHER_KEY), 300);
        let rendered = format!("{config:?}");
        let key_text = std::str::from_utf8(KEY).unwrap();
        let other_text = std::str::from_utf8(OTHER_KEY).unwrap();
        assert!(!rendered.contains(key_text), "Debug leaked the primary");
        assert!(!rendered.contains(other_text), "Debug leaked the secondary");
    }

    // --- env loading (process-global env: serialized by a mutex) ---

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env(vars: &[(&str, Option<&str>)], check: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        let all = [PRIMARY_SECRET_VAR, SECONDARY_SECRET_VAR, TOLERANCE_VAR];
        for var in all {
            // SAFETY: single-threaded within the ENV_LOCK critical section;
            // no other test touches these vars outside it.
            unsafe { std::env::remove_var(var) };
        }
        for (var, value) in vars {
            if let Some(value) = value {
                // SAFETY: as above.
                unsafe { std::env::set_var(var, value) };
            }
        }
        check();
        for var in all {
            // SAFETY: as above.
            unsafe { std::env::remove_var(var) };
        }
    }

    #[test]
    fn load_from_env_unset_is_feature_off() {
        with_env(&[], || {
            assert!(WebhookHmac::load_from_env().unwrap().is_none());
        });
    }

    #[test]
    fn load_from_env_short_primary_is_config_error() {
        // Empty hits the trim guard; "short" hits the constructor floor.
        // Both are distinct rejection paths, not a silent disable.
        with_env(&[(PRIMARY_SECRET_VAR, Some(""))], || {
            assert!(matches!(
                WebhookHmac::load_from_env().unwrap_err(),
                ConfigError::PrimaryTooShort { len: 0 }
            ));
        });
        with_env(&[(PRIMARY_SECRET_VAR, Some("short"))], || {
            assert!(matches!(
                WebhookHmac::load_from_env().unwrap_err(),
                ConfigError::PrimaryTooShort { len: 5 }
            ));
        });
    }

    #[test]
    fn load_from_env_secondary_without_primary_is_config_error() {
        with_env(
            &[(
                SECONDARY_SECRET_VAR,
                Some("0123456789abcdef0123456789abcdef"),
            )],
            || {
                assert!(matches!(
                    WebhookHmac::load_from_env().unwrap_err(),
                    ConfigError::SecondaryWithoutPrimary
                ));
            },
        );
    }

    #[test]
    fn load_from_env_full_configuration() {
        with_env(
            &[
                (PRIMARY_SECRET_VAR, Some("0123456789abcdef0123456789abcdef")),
                (
                    SECONDARY_SECRET_VAR,
                    Some("fedcba9876543210fedcba9876543210"),
                ),
                (TOLERANCE_VAR, Some("60")),
            ],
            || {
                let config = WebhookHmac::load_from_env().unwrap().unwrap();
                assert!(config.secondary.is_some());
                assert_eq!(config.tolerance(), Tolerance::new(60).unwrap());
            },
        );
    }

    #[test]
    fn load_from_env_default_tolerance() {
        with_env(
            &[(PRIMARY_SECRET_VAR, Some("0123456789abcdef0123456789abcdef"))],
            || {
                let config = WebhookHmac::load_from_env().unwrap().unwrap();
                assert_eq!(config.tolerance(), Tolerance::default());
            },
        );
    }

    #[test]
    fn load_from_env_malformed_and_out_of_range_tolerance() {
        with_env(
            &[
                (PRIMARY_SECRET_VAR, Some("0123456789abcdef0123456789abcdef")),
                (TOLERANCE_VAR, Some("five minutes")),
            ],
            || {
                assert!(matches!(
                    WebhookHmac::load_from_env().unwrap_err(),
                    ConfigError::MalformedTolerance
                ));
            },
        );
        with_env(
            &[
                (PRIMARY_SECRET_VAR, Some("0123456789abcdef0123456789abcdef")),
                (TOLERANCE_VAR, Some("86401")),
            ],
            || {
                assert!(matches!(
                    WebhookHmac::load_from_env().unwrap_err(),
                    ConfigError::ToleranceOutOfRange { secs: 86_401 }
                ));
            },
        );
    }
}
