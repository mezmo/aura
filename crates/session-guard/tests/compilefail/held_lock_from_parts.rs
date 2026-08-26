// HeldLock::from_parts must not be callable outside the crate.

fn main() {
    let _ = session_guard::HeldLock::from_parts;
}
