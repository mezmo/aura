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
