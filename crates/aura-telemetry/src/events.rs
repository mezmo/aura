//! Phase-1 event structs.
//!
//! Each one is a `#[derive(Event)]` struct whose fields are typed
//! against the [`IntoTelemetryProperty`](crate::IntoTelemetryProperty)
//! allow-list. Adding a property means extending the allow-list (which
//! a code reviewer will see).
//!
//! Every event listed in `docs/telemetry.md` has a row matching the
//! struct here. Phase 2+ adds: `server_shutdown`, `cli_command_invoked`,
//! `chat_request_started`, `chat_request_completed`, `tool_invoked`,
//! `orchestration_started`.

use aura_telemetry_derive::Event as DeriveEvent;

/// Emitted once per web-server boot, after logging is initialised and
/// before the HTTP listener is bound.
#[derive(DeriveEvent, Debug, Clone)]
#[aura_event(name = "server_started")]
pub struct ServerStarted {
    /// `true` if the operator supplied an explicit default agent in
    /// config; `false` if the default-agent fallback was used.
    pub default_agent_set: bool,
}

/// Emitted once per CLI invocation, after `AppConfig::load` returns.
#[derive(DeriveEvent, Debug, Clone)]
#[aura_event(name = "cli_session_started")]
pub struct CliSessionStarted {
    /// `true` if launched as a REPL (`aura`), `false` if one-shot
    /// (`aura -q "..."`).
    pub interactive: bool,
    /// `true` if `--standalone --config` (in-process agent builder)
    /// rather than the HTTP backend against `aura-web-server`.
    pub standalone_mode: bool,
    /// `true` if `--enable-client-tools` was passed (local shell-like
    /// tools advertised to the server). Affects audit posture; see
    /// `aura-cli/README.md`.
    pub client_tools_enabled: bool,
}
