//! Even `PropertyValue` itself is forbidden as a field type.
//!
//! The reason: `PropertyValue` is the union of *all* allowed property
//! variants, including envelope-level concepts. If a future variant is
//! added that should only ever appear on the envelope (e.g. an install
//! identifier), allowing `PropertyValue`-typed event fields would let
//! that variant be smuggled into the per-event property map by a future
//! contributor. Forcing fields to use one of the typed source types
//! (`bool`, `OsFamily`, `Source`, `DeploymentMethod`, `&'static str`)
//! keeps the gate structural rather than conventional.
//!
//! This file MUST fail to compile.

use aura_telemetry::{Event, PropertyValue};

#[derive(Event)]
#[aura_event(name = "leaky_event")]
struct LeakyEvent {
    smuggled: PropertyValue,
}

fn main() {}
