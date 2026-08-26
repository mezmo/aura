// HolderId::mint and OpId::mint must not be callable outside the crate.

fn main() {
    let _ = session_guard::HolderId::mint;
    let _ = session_guard::OpId::mint;
}
