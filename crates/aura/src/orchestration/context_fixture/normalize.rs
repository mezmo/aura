//! Test-side normalization of the two nondeterminism sources in the
//! envelope, and the snapshot assertion entry point.
//!
//! # Normalization design (pre-approved schema decision 2)
//!
//! Normalization is TEST-SIDE ONLY — no product code changes. Exactly two
//! rewrite classes are applied, and both are LOCATION-AWARE: they run over
//! the structured envelope (per message) BEFORE the snapshot document is
//! flattened, so a payload byte — a query, task result, playbook, or tool
//! description that happens to contain a marker — can never be rewritten.
//!
//! 1. **Live timestamps.** `build_planning_wrapper` and
//!    `build_continuation_wrapper` PREFIX their output with
//!    `Current time: <rfc3339>`. The rewrite to `Current time: <TIMESTAMP>`
//!    is anchored at byte offset 0 of a user-message body; the same text
//!    anywhere else is payload and is left untouched.
//! 2. **HashMap iteration order.** The worker roster surfaces iterate
//!    `OrchestrationConfig::workers` (a `HashMap`); they render ONLY inside
//!    the initial planning wrapper (the first user message): the
//!    `AVAILABLE WORKERS:` roster entries and the quoted names after
//!    `Valid worker names:`. Entries within exactly those spans of exactly
//!    that message are sorted lexicographically.
//!
//! # Occurrence audit (no silent rewrites, no silent skips)
//!
//! Before either pass rewrites anything, [`audit_normalization_markers`]
//! proves the envelope contains exactly the generated occurrences the
//! passes expect, and panics (normalization defect) otherwise:
//!
//! - user messages are all-or-none on the timestamp prefix: every user
//!   message starts with the full `Current time: <rfc3339>` prefix
//!   (coordinator envelopes) or none does (worker envelopes) — a mixed
//!   envelope means builder drift;
//! - a `Current time: ` prefix at offset 0 that does not parse as the full
//!   RFC3339 form is a defect, never a skip;
//! - the `AVAILABLE WORKERS:` and `Valid worker names:` markers appear at
//!   most once each, only in the first user message, and nowhere else in
//!   the envelope (fixture payloads must not embed the markers — the audit
//!   makes a collision a loud failure instead of a mis-sorted span).
//!
//! Any byte difference outside these two anchored classes survives
//! normalization and fails the snapshot — that is the point.

#![expect(
    dead_code,
    reason = "S2 type skeleton: the normalizer lands before the snapshot tests that consume it (S2 implementation step)"
)]
#![expect(
    unused_variables,
    reason = "S2 type skeleton: normalization bodies are todo!() until the S2 implementation step"
)]

use super::envelope::RequestEnvelope;

/// A snapshot-stable rendering of a [`RequestEnvelope`]: the three
/// envelope surfaces serialized to one labeled text document, with the two
/// normalization classes applied.
///
/// Business rule: snapshots compare normalized renderings, and
/// normalization applies exactly the two anchored rewrite classes named in
/// the module docs, after the occurrence audit proves only generated
/// occurrences are touched. Forbidden state: a "cleaned up" snapshot —
/// there is no generic scrubbing pass and no flattened-text rewrite, so
/// any envelope drift outside the two classes is a snapshot failure, not a
/// normalization artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NormalizedSnapshot(String);

impl NormalizedSnapshot {
    /// The snapshot text, as asserted by insta.
    pub(crate) fn as_str(&self) -> &str {
        todo!()
    }
}

/// The outcome of pass 1 on one user-message body.
enum TimestampScrub {
    /// The message began with the full prefix; it was rewritten.
    Scrubbed(String),
    /// The message carries no timestamp prefix at offset 0 (worker task
    /// prompt); the body is untouched.
    Absent,
}

/// Render an envelope to its normalized snapshot form: run
/// [`audit_normalization_markers`], apply the two per-message passes, then
/// serialize `SYSTEM`, `MESSAGES` (role-labeled, in order), and `TOOLS`
/// (canonical JSON) sections.
pub(crate) fn normalize(envelope: &RequestEnvelope) -> NormalizedSnapshot {
    todo!()
}

/// Occurrence audit run before any rewrite. Panics (normalization defect,
/// fail loud) when the envelope's marker occurrences differ from the
/// generated set the passes expect — mixed timestamp prefixes across user
/// messages, a malformed `Current time: ` prefix at offset 0, a roster
/// marker outside the first user message, or a duplicated roster marker
/// within it.
fn audit_normalization_markers(envelope: &RequestEnvelope) {
    todo!()
}

/// Pass 1 (location-aware): rewrite the `Current time: <rfc3339>` prefix
/// anchored at byte offset 0 of one user-message body to
/// `Current time: <TIMESTAMP>`. Occurrences anywhere else in the body are
/// payload bytes and are left untouched.
fn scrub_wrapper_timestamp(user_message: &str) -> TimestampScrub {
    todo!()
}

/// Pass 2 (location-aware): sort the HashMap-ordered worker spans of the
/// INITIAL PLANNING WRAPPER — roster entries under its `AVAILABLE WORKERS:`
/// heading (line entries for the None roster; `## name` blocks for
/// Summary/Full) and the quoted list after its `Valid worker names:` line —
/// lexicographically, leaving every other span of the message untouched.
/// Only ever applied to the first user message; rosters render nowhere
/// else.
fn canonicalize_worker_order(planning_wrapper: &str) -> String {
    todo!()
}

/// Assert the envelope's normalized snapshot against the committed
/// snapshot named `name` (insta, strict).
///
/// This is the byte-identity assertion mode for refactor cards S3-S6: run
/// with stale-snapshot updating disabled, an unchanged corpus proves
/// request-envelope identity over `MANIFEST.md`; any drift fails byte-for-
/// byte. The mode is proven by a no-op refactor in the S2 implementation
/// step.
pub(crate) fn assert_envelope_snapshot(name: &str, envelope: &RequestEnvelope) {
    todo!()
}
