// HeldParts must not be constructible outside the crate.

use session_guard::HeldParts;

fn main() {
    let _parts: HeldParts;
}
