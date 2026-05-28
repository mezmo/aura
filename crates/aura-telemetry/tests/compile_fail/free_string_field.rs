//! A `String` field is a free-form text channel — exactly the kind of
//! thing the anti-PII gate must reject. This file MUST fail to compile.

use aura_telemetry::Event;

#[derive(Event)]
#[aura_event(name = "leaky_event")]
struct LeakyEvent {
    raw_prompt: String,
}

fn main() {}
