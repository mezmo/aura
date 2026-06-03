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
         • It is enabled when you continue. To opt out, run `/telemetry disable` \
         (or set DO_NOT_TRACK=1) — nothing is sent until you do.",
    )
}

/// Print the one-time notice to the terminal.
pub(crate) fn present_notice() {
    println!("{}", notice_text().themed(AuraStyle::Muted));
    println!();
}

/// The first-input consent decision: does this first non-empty input
/// (entered after the notice was shown) enable telemetry?
///
/// Implied consent — any input that is **not** an explicit opt-out and
/// **not** an immediate quit enables telemetry. `/telemetry disable`
/// returns `false` so the state stays held and its normal command
/// dispatch records `Disabled`. `/quit` / `/exit` return `false` so a
/// user who bails leaves the state `Unknown` and sees the notice again
/// next launch (rather than being silently enabled on the way out).
pub(crate) fn first_input_enables_telemetry(input: &str) -> bool {
    let trimmed = input.trim();
    let is_opt_out = trimmed.starts_with("/telemetry disable");
    let is_quit = trimmed == "/quit" || trimmed == "/exit";
    !is_opt_out && !is_quit
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
    fn normal_first_input_enables() {
        assert!(first_input_enables_telemetry("what is my cpu usage?"));
        assert!(first_input_enables_telemetry("/help"));
        assert!(first_input_enables_telemetry("/telemetry status"));
    }

    #[test]
    fn explicit_opt_out_does_not_enable() {
        assert!(!first_input_enables_telemetry("/telemetry disable"));
        assert!(!first_input_enables_telemetry("  /telemetry disable  "));
    }

    #[test]
    fn quitting_does_not_enable() {
        assert!(!first_input_enables_telemetry("/quit"));
        assert!(!first_input_enables_telemetry("/exit"));
    }
}
