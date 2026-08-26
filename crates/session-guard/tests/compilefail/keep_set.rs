// KeepSet must not be nameable or constructible outside the crate.

fn main() {
    let _ = session_guard::KeepSet::from_manifest;
}
