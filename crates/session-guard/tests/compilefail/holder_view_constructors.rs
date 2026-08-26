// HolderView constructors must not be callable outside the crate.

fn main() {
    let _ = session_guard::HolderView::here;
    let _ = session_guard::HolderView::remote;
}
