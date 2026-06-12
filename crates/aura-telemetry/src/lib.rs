//! Aura product telemetry.
//!
//! Opt-out, anonymous-tier behavioural analytics. See `docs/telemetry.md`
//! for the user-facing contract (what is collected, what is not, kill
//! switches, self-hosted sink).
//!
//! The entire telemetry surface lives in this crate so it can be audited
//! in isolation:
//!
//! - [`disable`] — the kill-switch decision tree.
//! - [`properties`] — sealed enum of every property value ever sent,
//!   and the [`properties::IntoTelemetryProperty`] trait that gates which
//!   Rust types may appear on an event struct.
//! - [`Event`] / [`EventPayload`] — the typed-event abstraction. Use
//!   `#[derive(Event)]` (from `aura-telemetry-derive`) to define events;
//!   any field whose type does not implement
//!   `properties::IntoTelemetryProperty` fails to compile.
//! - [`install_id`] — anonymous install UUID persisted on disk; sets
//!   PostHog `distinct_id` and never appears in event property maps.

// Let `#[derive(Event)]` resolve `aura_telemetry::...` paths even when
// the macro is used inside this crate's own modules (e.g. `events.rs`).
extern crate self as aura_telemetry;

pub mod bootstrap;
pub mod disable;
pub mod events;
pub mod handle;
pub mod inspection_log;
pub mod install_id;
pub mod properties;
pub mod sink;

pub use disable::{decide_state, DisableReason, EnvProvider, TelemetryState};
pub use handle::{init, EnableOutcome, FileTelemetryConfig, TelemetryConfig, TelemetryHandle};
pub use properties::{IntoTelemetryProperty, Properties, PropertyValue};

pub use aura_telemetry_derive::Event;

/// HTTP header an **Enabled** CLI attaches to its requests to propagate
/// the user's telemetry consent to the non-interactive server it drives.
/// Consent is **request-scoped**: server-side per-request events are
/// captured via [`TelemetryHandle::capture_consented`], which sends only
/// telemetry about that request and never changes the server's global
/// state. A `Disabled` server ignores it (operator opt-out wins). See
/// `docs/telemetry.md`.
pub const CONSENT_HEADER: &str = "x-aura-telemetry-consent";

/// The only value [`CONSENT_HEADER`] is ever sent with.
pub const CONSENT_HEADER_VALUE: &str = "enabled";

/// User-facing message for when the local inspection log is disabled via
/// `AURA_TELEMETRY_LOG_EVENTS`. Shared so the CLI and web server (and any
/// future surface) describe the condition identically — and stay in sync
/// if the env var is ever renamed.
pub const INSPECTION_LOG_DISABLED_MSG: &str =
    "inspection log disabled (AURA_TELEMETRY_LOG_EVENTS=0)";

/// The wire-ready property bag for a single event. The macro builds this
/// from each `#[derive(Event)]` struct. `Clone` is supported so the
/// background-task channel can move owned copies.
#[derive(Debug, Clone)]
pub struct EventPayload {
    pub name: &'static str,
    pub properties: Properties,
}

/// All telemetry events implement this. Production code uses
/// `#[derive(Event)]`; the trait is also implementable by hand for
/// special cases (none today).
pub trait Event: Sized {
    const NAME: &'static str;
    fn into_payload(self) -> EventPayload;
}
