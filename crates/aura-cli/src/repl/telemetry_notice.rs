//! First-run telemetry notice.
//!
//! Presented once, in the interactive REPL, the first time AURA runs
//! with no recorded telemetry preference (state == Unknown). It is the
//! consent gate required by the spec: it states that telemetry is
//! collected, links to the documentation describing the schema /
//! exclusions / controls, and explains how to disable before anything
//! is sent. Telemetry remains **held** until the user's first
//! non-opt-out input (see the first-input gate in `repl::loop`).

use crate::theme::{AuraStyle, Themed};
use aura_telemetry::TelemetryState;

/// The documentation URL shown in the notice. The repo path is also
/// valid for local checkouts; this is the canonical published location.
const TELEMETRY_DOCS_URL: &str = "https://github.com/mezmo/aura/blob/main/docs/telemetry.md";

/// Render the notice body as a string (separated from printing so it can
/// be unit-tested).
pub(crate) fn notice_text() -> String {
    format!(
        "AURA collects anonymous usage telemetry to help maintainers \
         understand how it is used.\n\
         • What is and isn't collected, and how to control it: {TELEMETRY_DOCS_URL}\n\
         • It is enabled when you send your first message. To opt out, run \
         `/telemetry disable` (or set DO_NOT_TRACK=1) — nothing is sent until you do.",
    )
}

/// Print the one-time notice to the terminal.
pub(crate) fn present_notice() {
    println!("{}", notice_text().themed(AuraStyle::Muted));
    println!();
}

/// What the consent gate should do when the user sends their first chat
/// message after the notice was shown.
#[derive(Debug)]
pub(crate) enum FirstMessageConsent {
    /// No preference recorded yet: enable, persist `enabled = true`, and
    /// capture the session-start event.
    EnableAndCapture,
    /// The user already ran `/telemetry enable` this session — dispatch
    /// enabled and persisted; only the session-start event is still due.
    CaptureOnly,
    /// A kill switch or explicit opt-out is in effect; do nothing.
    Skip,
}

/// First-message consent decision, derived from the telemetry *state*
/// rather than by re-parsing the input string.
///
/// Implied consent happens only at the point where input is actually
/// submitted to the agent. Slash commands — including `/telemetry status`,
/// typos, unknown commands, and `/quit` — never grant consent: they are
/// dispatched before this gate runs, and whatever state they leave
/// behind (`Disabled` after `/telemetry disable`, `Enabled` after
/// `/telemetry enable`, otherwise still `Unknown`) is what decides here.
/// This keeps the gate from drifting out of sync with the command
/// dispatcher's parsing, and means a user who only inspects (`status`,
/// `/help`) or bails (`/quit`) keeps the notice for next launch.
pub(crate) fn consent_on_first_message(state: &TelemetryState) -> FirstMessageConsent {
    match state {
        TelemetryState::Unknown => FirstMessageConsent::EnableAndCapture,
        TelemetryState::Enabled => FirstMessageConsent::CaptureOnly,
        TelemetryState::Disabled(_) => FirstMessageConsent::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_states_collection_links_docs_and_offers_optout() {
        let t = notice_text();
        assert!(t.contains("telemetry"));
        assert!(t.contains("docs/telemetry.md"));
        assert!(t.contains("/telemetry disable") || t.contains("DO_NOT_TRACK"));
        // The notice must make clear nothing is sent until the user acts.
        assert!(t.contains("nothing is sent"));
    }

    #[test]
    fn unknown_state_enables_and_captures() {
        assert!(matches!(
            consent_on_first_message(&TelemetryState::Unknown),
            FirstMessageConsent::EnableAndCapture
        ));
    }

    #[test]
    fn explicitly_enabled_state_captures_only() {
        // The user ran `/telemetry enable` before their first message;
        // dispatch already enabled and persisted, so the gate must not
        // re-enable — but the session-start event is still due.
        assert!(matches!(
            consent_on_first_message(&TelemetryState::Enabled),
            FirstMessageConsent::CaptureOnly
        ));
    }

    #[test]
    fn disabled_state_skips() {
        // An explicit opt-out (`/telemetry disable`, kill switch) before
        // the first message stands: the gate must never override it.
        for reason in [
            aura_telemetry::DisableReason::AuraDisabled,
            aura_telemetry::DisableReason::DoNotTrack,
        ] {
            assert!(matches!(
                consent_on_first_message(&TelemetryState::Disabled(reason)),
                FirstMessageConsent::Skip
            ));
        }
    }
}
