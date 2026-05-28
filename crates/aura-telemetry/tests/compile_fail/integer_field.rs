//! Numeric fields can leak counts and quasi-identifiers (PIDs, ports,
//! request sizes, line offsets in user files). The anti-PII gate
//! requires that they be wrapped in a typed bucket enum first. This
//! file MUST fail to compile.

use aura_telemetry::Event;

#[derive(Event)]
#[aura_event(name = "leaky_event")]
struct LeakyEvent {
    request_size_bytes: u64,
}

fn main() {}
