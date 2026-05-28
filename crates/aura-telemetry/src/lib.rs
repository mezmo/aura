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

pub mod disable;
pub mod properties;

pub use disable::{decide_disabled, DisableReason, EnvProvider};
pub use properties::{IntoTelemetryProperty, Properties, PropertyValue};

pub use aura_telemetry_derive::Event;

/// The wire-ready property bag for a single event. The macro builds this
/// from each `#[derive(Event)]` struct.
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
