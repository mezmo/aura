//! Telemetry event structs.
//!
//! Each one is a `#[derive(Event)]` struct whose fields are typed
//! against the [`IntoTelemetryProperty`](crate::IntoTelemetryProperty)
//! allow-list. Adding a property means extending the allow-list (which
//! a code reviewer will see).
//!
//! Every struct here documents — per the project's telemetry ADR
//! (`docs/adr/2026-06-23-cli-product-telemetry.md`) — **why** we track
//! it and **how** the signal will be used to improve aura for users. We
//! do not add an event or a field without a concrete improvement
//! hypothesis. Every event also has a matching row in `docs/telemetry.md`.

use aura_telemetry_derive::Event as DeriveEvent;

/// Emitted once per interactive CLI session, when the user grants consent
/// by sending their first chat message (or immediately on launch if
/// telemetry was already `Enabled`).
///
/// **Why we track it / how we use it:** the run-mode mix tells us where
/// to spend UX and performance effort. If most sessions are
/// `standalone_mode`, that path deserves the investment; if
/// `client_tools_enabled` is common we prioritise the audit/safety
/// documentation around it. Without this we would be guessing which
/// surface real users actually run.
#[derive(DeriveEvent, Debug, Clone)]
#[aura_event(name = "cli_session_started")]
pub struct CliSessionStarted {
    /// `true` if launched as a REPL (`aura`), `false` if one-shot
    /// (`aura -q "..."`). One-shot stays held and never sends, so in
    /// practice this is `true` on the wire; the field documents the
    /// surface and guards against future one-shot consent paths.
    pub interactive: bool,
    /// `true` if `--standalone --config` (in-process agent builder)
    /// rather than the HTTP backend against `aura-web-server`.
    pub standalone_mode: bool,
    /// `true` if `--enable-client-tools` was passed (local shell-like
    /// tools advertised to the server). Affects audit posture; see
    /// `aura-cli/README.md`.
    pub client_tools_enabled: bool,
}

/// Emitted at the start of each logical chat turn in the REPL, once per
/// turn regardless of backend (HTTP or standalone).
///
/// **Why we track it / how we use it:** turn volume is the core
/// production-adoption signal — it answers "are people actually using
/// aura-cli, and how much?" That signal is what justifies continued
/// investment in the CLI. Carries no properties beyond the anonymous
/// envelope: no prompt, no content, no model, no counts.
#[derive(DeriveEvent, Debug, Clone)]
#[aura_event(name = "chat_request_started")]
pub struct ChatRequestStarted {}

/// Emitted when a chat turn finishes, paired 1:1 with
/// [`ChatRequestStarted`], once per turn regardless of backend.
///
/// **Why we track it / how we use it:** the success rate (completed vs
/// started) surfaces reliability regressions we should fix — a drop in
/// success is a direct quality signal. We deliberately do **not** carry
/// latency, model id, token counts, or any error detail in v1; only the
/// boolean outcome.
#[derive(DeriveEvent, Debug, Clone)]
#[aura_event(name = "chat_request_completed")]
pub struct ChatRequestCompleted {
    /// `true` if the turn completed without an error surfacing to the
    /// user; `false` if it errored or was cancelled.
    pub success: bool,
}
