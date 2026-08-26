// Frames for DESIGN.md sections:
// - artifact_path_parse_rules: Type-to-business-rule map: ArtifactPath
// - digest_from_hex_rules: Type-to-business-rule map: Digest
// - manifest_serde_empty_fresh_row: Admission protocol / Manifest (wire-pin: green by design)
// - epoch_deserialize_rejects_zero: Type-to-business-rule map: Epoch (wire-pin: green by design)
// - digest_display_from_bytes: Type-to-business-rule map: Digest (wire-pin: green by design)

use std::panic::{AssertUnwindSafe, catch_unwind};

use session_guard::{ArtifactPath, Digest, Epoch, Manifest};

fn todo_message(payload: &dyn std::any::Any) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default()
}

fn expect_todo_panic(result: Result<(), Box<dyn std::any::Any + Send>>) {
    if let Err(payload) = result {
        let msg = todo_message(&*payload);
        assert!(msg.contains("fill:"), "unexpected panic: {msg:?}");
        panic!("waits todo!(): {msg}");
    }
}

#[test]
fn artifact_path_parse_rules() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let p = ArtifactPath::parse("e1/foo/bar.txt").expect("epoch-qualified path parses");
        assert_eq!(p.to_string(), "e1/foo/bar.txt");
        assert_eq!(p.epoch().as_u64(), 1);

        assert!(ArtifactPath::parse("foo/bar").is_err());
        assert!(ArtifactPath::parse("e0/foo").is_err());
        assert!(ArtifactPath::parse("e1/../x").is_err());
        assert!(ArtifactPath::parse("e1/./x").is_err());
        assert!(ArtifactPath::parse("/e1/foo").is_err());
        assert!(ArtifactPath::parse("e01/foo.txt").is_err());
        assert!(ArtifactPath::parse("e1").is_err());
    }));
    expect_todo_panic(result);
}

#[test]
fn digest_from_hex_rules() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let hex = "0".repeat(64);
        let d = Digest::from_hex(&hex).expect("64 lowercase hex parses");
        assert_eq!(d.to_string(), hex);

        assert!(Digest::from_hex("").is_err());
        assert!(Digest::from_hex("0").is_err());
        assert!(Digest::from_hex("g".repeat(64).as_str()).is_err());
        assert!(Digest::from_hex("0".repeat(63).as_str()).is_err());
        assert!(Digest::from_hex("0".repeat(65).as_str()).is_err());
    }));
    expect_todo_panic(result);
}

#[test]
fn manifest_serde_empty_fresh_row() {
    // wire-pin: green by design (existing derive), not a frame
    let manifest: Manifest =
        serde_json::from_str("{}").expect("fresh-row '{}' jsonb deserializes to empty manifest");
    assert!(manifest.is_empty());
    assert_eq!(serde_json::to_string(&manifest).unwrap(), "{}");
}

#[test]
fn epoch_deserialize_rejects_zero() {
    // wire-pin: green by design (existing Deserialize validation), not a frame
    let valid: Epoch = serde_json::from_str("1").expect("epoch 1 deserializes");
    assert_eq!(valid.as_u64(), 1);
    let zero: Result<Epoch, _> = serde_json::from_str("0");
    assert!(zero.is_err());
}

#[test]
fn digest_display_from_bytes() {
    // wire-pin: green by design (existing Display + const constructor), not a frame
    let d = Digest::from_bytes([0xab; 32]);
    assert_eq!(d.to_string(), "ab".repeat(32));
}
