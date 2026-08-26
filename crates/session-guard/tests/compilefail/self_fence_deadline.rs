// SelfFenceDeadline constructors must not be callable outside the crate.

fn main() {
    let _ = session_guard::SelfFenceDeadline::fenced;
    let _ = session_guard::SelfFenceDeadline::anchor_at;
}
