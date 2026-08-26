// SelfFenceDeadline constructors must not be callable outside the crate.
// Reference the type through the private `lease` module so the error is a
// path-privacy violation, not a "similar name" suggestion.

fn main() {
    let _ = session_guard::lease::SelfFenceDeadline::fenced;
    let _ = session_guard::lease::SelfFenceDeadline::anchor_at;
}
