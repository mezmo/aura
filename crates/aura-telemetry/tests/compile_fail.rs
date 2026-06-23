//! Compile-fail tests that lock in the anti-PII gate at the type level.
//!
//! Each case under `tests/compile_fail/` is a tiny program that attempts to
//! derive `Event` on a struct with a forbidden field type. The build
//! must fail; `trybuild` asserts the failure message against a `.stderr`
//! snapshot so a future loosening of the gate would break the build.

#[test]
fn forbidden_field_types_fail_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
