// HeartbeatLease constructors must not be callable outside the crate.

fn main() {
    let _ = session_guard::HeartbeatLease::static_from;
    let _ = session_guard::HeartbeatLease::with_heartbeat;
}
