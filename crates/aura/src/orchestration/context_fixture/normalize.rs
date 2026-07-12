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

use rig::completion::Message;
use rig::completion::message::{AssistantContent, UserContent};

use super::envelope::RequestEnvelope;

/// The timestamp label both wrappers prefix their output with.
const TIMESTAMP_LABEL: &str = "Current time: ";
/// The stand-in written by pass 1.
const TIMESTAMP_STAND_IN: &str = "Current time: <TIMESTAMP>";
/// Byte length of the RFC3339 seconds-precision Zulu form the wrappers
/// emit (`2026-07-08T00:33:54Z`).
const RFC3339_LEN: usize = 20;
/// Pass-2 markers: the roster heading and the valid-names label.
const ROSTER_MARKER: &str = "AVAILABLE WORKERS:";
const VALID_NAMES_MARKER: &str = "Valid worker names: ";

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
        &self.0
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

/// The role label and text body of one envelope message. The conversation
/// at `9df96382` holds only single-part text turns; anything else is
/// builder drift and panics.
fn message_role_and_text(message: &Message) -> (&'static str, String) {
    match message {
        Message::User { content } => {
            let mut parts = content.iter();
            match (parts.next(), parts.next()) {
                (Some(UserContent::Text(text)), None) => ("user", text.text.clone()),
                _ => panic!(
                    "envelope user messages are single text parts at 9df96382; got {message:?}"
                ),
            }
        }
        Message::Assistant { content, .. } => {
            let mut parts = content.iter();
            match (parts.next(), parts.next()) {
                (Some(AssistantContent::Text(text)), None) => ("assistant", text.text.clone()),
                _ => panic!(
                    "envelope assistant messages are single text parts at 9df96382; got {message:?}"
                ),
            }
        }
    }
}

/// Render an envelope to its normalized snapshot form: run
/// [`audit_normalization_markers`], apply the two per-message passes, then
/// serialize `SYSTEM`, `MESSAGES` (role-labeled, in order), and `TOOLS`
/// (canonical JSON) sections.
pub(crate) fn normalize(envelope: &RequestEnvelope) -> NormalizedSnapshot {
    audit_normalization_markers(envelope);

    let mut document = String::new();
    document.push_str("================ SYSTEM ================\n");
    document.push_str(&envelope.system);
    document.push_str("\n\n================ MESSAGES ================\n");

    let mut first_user_seen = false;
    for (index, message) in envelope.messages.iter().enumerate() {
        let (role, text) = message_role_and_text(message);
        let mut body = text;
        if role == "user" {
            if let TimestampScrub::Scrubbed(scrubbed) = scrub_wrapper_timestamp(&body) {
                body = scrubbed;
            }
            if !first_user_seen {
                first_user_seen = true;
                if body.contains(ROSTER_MARKER) || body.contains(VALID_NAMES_MARKER) {
                    body = canonicalize_worker_order(&body);
                }
            }
        }
        document.push_str(&format!("---- [{index}] {role} ----\n{body}\n\n"));
    }

    document.push_str("================ TOOLS ================\n");
    document.push_str(
        &serde_json::to_string_pretty(&envelope.tools_json()).expect("tools JSON renders"),
    );
    document.push('\n');

    NormalizedSnapshot(document)
}

/// Whether `body` starts with the full generated timestamp prefix
/// (`Current time: ` + RFC3339 seconds-precision Zulu form).
fn has_full_timestamp_prefix(body: &str) -> bool {
    let Some(rest) = body.strip_prefix(TIMESTAMP_LABEL) else {
        return false;
    };
    let Some(stamp) = rest.get(..RFC3339_LEN) else {
        return false;
    };
    let bytes = stamp.as_bytes();
    bytes.iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        10 => *b == b'T',
        13 | 16 => *b == b':',
        19 => *b == b'Z',
        _ => b.is_ascii_digit(),
    })
}

/// Occurrence audit run before any rewrite. Panics (normalization defect,
/// fail loud) when the envelope's marker occurrences differ from the
/// generated set the passes expect — mixed timestamp prefixes across user
/// messages, a malformed `Current time: ` prefix at offset 0, a roster
/// marker outside the first user message, or a duplicated roster marker
/// within it.
fn audit_normalization_markers(envelope: &RequestEnvelope) {
    let mut user_bodies = Vec::new();
    let mut assistant_bodies = Vec::new();
    for message in &envelope.messages {
        let (role, text) = message_role_and_text(message);
        if role == "user" {
            user_bodies.push(text);
        } else {
            assistant_bodies.push(text);
        }
    }

    let with_prefix = user_bodies
        .iter()
        .filter(|body| has_full_timestamp_prefix(body))
        .count();
    for body in &user_bodies {
        assert!(
            !body.starts_with(TIMESTAMP_LABEL) || has_full_timestamp_prefix(body),
            "normalization defect: user message starts with a malformed \
             'Current time: ' prefix: {:?}",
            &body[..body.len().min(60)]
        );
    }
    assert!(
        with_prefix == 0 || with_prefix == user_bodies.len(),
        "normalization defect: {} of {} user messages carry the timestamp prefix \
         (all-or-none expected; mixed prefixes mean builder drift)",
        with_prefix,
        user_bodies.len()
    );

    let count = |haystack: &str, needle: &str| haystack.matches(needle).count();
    let tools_json = serde_json::to_string(&envelope.tools_json()).expect("tools JSON renders");
    for marker in [ROSTER_MARKER, VALID_NAMES_MARKER] {
        assert_eq!(
            count(&envelope.system, marker),
            0,
            "normalization defect: {marker:?} found in the system preamble (payload collision)"
        );
        assert_eq!(
            count(&tools_json, marker),
            0,
            "normalization defect: {marker:?} found in the tools JSON (payload collision)"
        );
        for body in &assistant_bodies {
            assert_eq!(
                count(body, marker),
                0,
                "normalization defect: {marker:?} found in an assistant turn (payload collision)"
            );
        }
        for (index, body) in user_bodies.iter().enumerate() {
            let occurrences = count(body, marker);
            if index == 0 {
                assert!(
                    occurrences <= 1,
                    "normalization defect: {marker:?} appears {occurrences} times in the \
                     initial planning wrapper (at most once expected)"
                );
            } else {
                assert_eq!(
                    occurrences, 0,
                    "normalization defect: {marker:?} found in user message {index} \
                     (rosters render only in the initial planning wrapper)"
                );
            }
        }
    }
}

/// Pass 1 (location-aware): rewrite the `Current time: <rfc3339>` prefix
/// anchored at byte offset 0 of one user-message body to
/// `Current time: <TIMESTAMP>`. Occurrences anywhere else in the body are
/// payload bytes and are left untouched.
fn scrub_wrapper_timestamp(user_message: &str) -> TimestampScrub {
    if !has_full_timestamp_prefix(user_message) {
        return TimestampScrub::Absent;
    }
    let rest = &user_message[TIMESTAMP_LABEL.len() + RFC3339_LEN..];
    TimestampScrub::Scrubbed(format!("{TIMESTAMP_STAND_IN}{rest}"))
}

/// Sort the lines of one roster span lexicographically, preserving the
/// span's surrounding text.
fn sort_span(message: &str, start: usize, end: usize, separator: &str) -> String {
    let mut entries: Vec<&str> = message[start..end].split(separator).collect();
    entries.sort_unstable();
    format!(
        "{}{}{}",
        &message[..start],
        entries.join(separator),
        &message[end..]
    )
}

/// Pass 2 (location-aware): sort the HashMap-ordered worker spans of the
/// INITIAL PLANNING WRAPPER — roster entries under its `AVAILABLE WORKERS:`
/// heading (line entries for the None roster; `## name` blocks for
/// Summary/Full) and the quoted list after its `Valid worker names:` line —
/// lexicographically, leaving every other span of the message untouched.
/// Only ever applied to the first user message; rosters render nowhere
/// else.
fn canonicalize_worker_order(planning_wrapper: &str) -> String {
    let mut message = planning_wrapper.to_owned();

    if let Some(heading) = message.find(ROSTER_MARKER) {
        let after_heading = heading + ROSTER_MARKER.len() + 1;
        // Summary/Full rosters open with the fixed NOTE line and close with
        // the fixed assignment sentence; the None roster is bare `- name:`
        // lines closed by the fixed capabilities sentence.
        const NOTE_PREFIX: &str = "NOTE: ";
        const INLINE_TAIL: &str =
            "\n\nAssign tasks to the worker whose tools best match the required operations.";
        const NO_TOOLS_TAIL: &str = "\n\nEach worker has specialized capabilities. Assign tasks to the most appropriate worker.";
        if message[after_heading..].starts_with(NOTE_PREFIX) {
            let note_end = message[after_heading..]
                .find("\n\n")
                .map(|i| after_heading + i + 2)
                .expect("normalization defect: Summary/Full roster NOTE line has no terminator");
            let span_end = message[note_end..]
                .find(INLINE_TAIL)
                .map(|i| note_end + i)
                .expect("normalization defect: Summary/Full roster has no closing sentence");
            for block in message[note_end..span_end].split("\n\n") {
                assert!(
                    block.starts_with("## "),
                    "normalization defect: roster block does not start with '## ': {block:?}"
                );
            }
            message = sort_span(&message, note_end, span_end, "\n\n");
        } else {
            let span_end = message[after_heading..]
                .find(NO_TOOLS_TAIL)
                .map(|i| after_heading + i)
                .expect("normalization defect: None-visibility roster has no closing sentence");
            message = sort_span(&message, after_heading, span_end, "\n");
        }
    }

    if let Some(label) = message.find(VALID_NAMES_MARKER) {
        let names_start = label + VALID_NAMES_MARKER.len();
        let names_end = message[names_start..]
            .find('\n')
            .map(|i| names_start + i)
            .expect("normalization defect: valid-names line has no terminator");
        message = sort_span(&message, names_start, names_end, ", ");
    }

    message
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
    let snapshot = normalize(envelope);
    insta::assert_snapshot!(name, snapshot.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::ToolDefinition;

    fn envelope(messages: Vec<Message>) -> RequestEnvelope {
        RequestEnvelope {
            system: "SYSTEM TEXT".to_owned(),
            messages,
            tools: vec![ToolDefinition {
                name: "t".to_owned(),
                description: "d".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        }
    }

    #[test]
    fn timestamp_prefix_is_scrubbed_only_at_offset_zero() {
        let body = "Current time: 2026-07-08T00:33:54Z\n\npayload says Current time: 2026-07-08T00:33:54Z too";
        let normalized = normalize(&envelope(vec![Message::user(body)]));
        let text = normalized.as_str();
        assert!(text.contains("Current time: <TIMESTAMP>\n\npayload"));
        assert!(
            text.contains("payload says Current time: 2026-07-08T00:33:54Z too"),
            "payload occurrence must survive: {text}"
        );
    }

    #[test]
    #[should_panic(expected = "malformed")]
    fn malformed_timestamp_prefix_panics_instead_of_skipping() {
        let body = "Current time: not-a-timestamp\n\nrest";
        normalize(&envelope(vec![Message::user(body)]));
    }

    #[test]
    #[should_panic(expected = "all-or-none")]
    fn mixed_timestamp_prefixes_panic() {
        normalize(&envelope(vec![
            Message::user("Current time: 2026-07-08T00:33:54Z\n\nfirst"),
            Message::assistant("decision"),
            Message::user("no prefix here"),
        ]));
    }

    #[test]
    #[should_panic(expected = "payload collision")]
    fn roster_marker_outside_first_user_message_panics() {
        normalize(&envelope(vec![
            Message::user("first"),
            Message::assistant("AVAILABLE WORKERS: echoed into a turn"),
        ]));
    }

    #[test]
    fn worker_envelope_without_markers_passes_untouched() {
        let normalized = normalize(&envelope(vec![Message::user("YOUR TASK: do the thing")]));
        assert!(normalized.as_str().contains("YOUR TASK: do the thing"));
    }
}
