// Frames for DESIGN.md sections:
// - session_id_rules: Type-to-business-rule map: SessionId
// - turn_id_parse_wire: Type-to-business-rule map: TurnId
// - pod_id_parse_rules: Type-to-business-rule map: PodId
// - holder_id_parse_wire: Type-to-business-rule map: HolderId
// - op_id_parse_wire: Type-to-business-rule map: OpId
// - pg_url_scheme_check: Type-to-business-rule map: PgUrl
// - build_admission_off_dispatch: Type-to-business-rule map: build_admission / AdmissionMode
//
// (pod_id_from_env_chain and admission_env_validation are reblocked by
// forbid(unsafe_code); see STOP-REPORT.md.)

use std::panic::{AssertUnwindSafe, catch_unwind};

use session_guard::{
    AdmissionEnv, AdmissionMode, HolderId, OpId, PgUrl, PodId, SessionId, TurnId, build_admission,
};

fn assert_clean_admission_env() {
    for var in [
        "AURA_SESSION_ADMISSION",
        "AURA_SESSION_ADMISSION_PG_URL",
        "AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS",
        "AURA_SESSION_ADMISSION_LEASE_TTL_MS",
        "AURA_SESSION_ADMISSION_FENCE_MARGIN_MS",
        "AURA_SESSION_ADMISSION_RETRY_AFTER_MS",
        "AURA_SESSION_ADMISSION_PROPAGATION_WINDOW_MS",
        "AURA_SESSION_REPAIR_LANE",
    ] {
        if std::env::var_os(var).is_some() {
            panic!("unset {var} to run these frames");
        }
    }
}

fn expect_todo_panic_for(
    result: Result<(), Box<dyn std::any::Any + Send>>,
    frame: &str,
    target: &str,
    fill: &str,
) {
    if result.is_ok() {
        return;
    }
    panic!("frame {frame}: waits todo!(): {target} (fill {fill})");
}

fn expect_todo_panic_with_domino(
    result: Result<(), Box<dyn std::any::Any + Send>>,
    frame: &str,
    target: &str,
    fill: &str,
) {
    if result.is_ok() {
        return;
    }
    let payload = result.unwrap_err();
    let upstream = payload
        .downcast_ref::<String>()
        .map(|s| s.as_str().to_string())
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    panic!(
        "frame {}: waits todo!(): {} (fill {}); upstream domino: {}",
        frame, target, fill, upstream
    );
}

#[test]
fn session_id_rules() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        assert!(SessionId::parse("abc-123").is_ok());
        assert!(SessionId::parse("A.B-C_1").is_ok());
        assert!(SessionId::parse("").is_err());
        assert!(SessionId::parse(".").is_err());
        assert!(SessionId::parse("..").is_err());
        assert!(SessionId::parse("latest").is_err());
        assert!(SessionId::parse("a/b").is_err());
        let long = "x".repeat(129);
        assert!(SessionId::parse(&long).is_err());
    }));
    expect_todo_panic_for(result, "session_id_rules", "SessionId::parse", "u1");
}

#[test]
fn turn_id_parse_wire() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let uuid = "018f1234-5678-7abc-8def-0123456789ab";
        let tid = TurnId::parse(uuid).expect("turn id parses wire uuid");
        assert_eq!(tid.to_string(), uuid);
        assert!(TurnId::parse("not-a-uuid").is_err());
    }));
    expect_todo_panic_for(result, "turn_id_parse_wire", "TurnId::parse", "u1");
}

#[test]
fn pod_id_parse_rules() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        assert!(PodId::parse("pod-1").is_ok());
        assert!(PodId::parse("pod.name-2").is_ok());
        assert!(PodId::parse("").is_err());
        assert!(PodId::parse("pod/name").is_err());
        assert!(PodId::parse("pod_name").is_err());
        assert!(PodId::parse("Pod.Name-2").is_err());
        let long = "p".repeat(254);
        assert!(PodId::parse(&long).is_err());
    }));
    expect_todo_panic_for(result, "pod_id_parse_rules", "PodId::parse", "u1");
}

#[test]
fn holder_id_parse_wire() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let uuid = "018f1234-5678-7abc-8def-0123456789ab";
        let h = HolderId::parse(uuid).expect("holder id parses wire uuid");
        assert_eq!(h.to_string(), uuid);
        assert!(HolderId::parse("bad").is_err());
    }));
    expect_todo_panic_for(result, "holder_id_parse_wire", "HolderId::parse", "u1");
}

#[test]
fn op_id_parse_wire() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let uuid = "018f1234-5678-7abc-8def-0123456789ab";
        let o = OpId::parse(uuid).expect("op id parses wire uuid");
        assert_eq!(o.to_string(), uuid);
        assert!(OpId::parse("bad").is_err());
    }));
    expect_todo_panic_for(result, "op_id_parse_wire", "OpId::parse", "u1");
}

#[test]
fn pg_url_scheme_check() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        assert!(PgUrl::parse("postgres://host/db").is_ok());
        assert!(PgUrl::parse("postgresql://host/db").is_ok());
        assert!(PgUrl::parse("http://host/db").is_err());
        assert!(PgUrl::parse("").is_err());
    }));
    expect_todo_panic_for(result, "pg_url_scheme_check", "PgUrl::parse", "u1");
}

#[test]
fn build_admission_off_dispatch() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let pod = PodId::parse("pod-0").expect("pod id parses");
        let env = AdmissionEnv::from_env().expect("config parses");
        assert_eq!(env.mode(), AdmissionMode::Off);
        let root = tempfile::tempdir().expect("temp root");
        let _admission = build_admission(&env, root.path().to_path_buf(), pod)
            .expect("factory dispatches off mode to a LocalAdmission");
    }));
    expect_todo_panic_with_domino(
        result,
        "build_admission_off_dispatch",
        "build_admission",
        "u1",
    );
}
