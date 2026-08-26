// Epoch constructors must not be callable outside the crate.

fn main() {
    let _ = session_guard::Epoch::initial;
    let _ = session_guard::Epoch::next;
    let _ = session_guard::Epoch::from_raw;
}
