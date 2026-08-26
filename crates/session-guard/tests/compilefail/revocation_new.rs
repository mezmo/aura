// Revocation::new must not be callable outside the crate.

fn main() {
    let _ = session_guard::Revocation::new;
}
