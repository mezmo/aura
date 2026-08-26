// Frames for DESIGN.md sections:
// - local_admission_admit_and_locate: Type-to-business-rule map: LocalAdmission / TurnAdmission
// - local_admission_same_session_rejected: Type-to-business-rule map: SessionArbiter (same-instance serialization)

use std::panic::{AssertUnwindSafe, catch_unwind};

use session_guard::{
    AdmissionEnv, IdleRequest, LeaseState, Locality, PodId, SessionId, TurnId, build_admission,
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

fn expect_todo_panic_with_domino(
    result: Result<(), Box<dyn std::any::Any + Send>>,
    frame: &str,
    target: &str,
    fill: &str,
) {
    if result.is_ok() {
        return;
    }
    let upstream = result
        .unwrap_err()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .unwrap_or_default();
    panic!(
        "frame {frame}: waits todo!(): {target} (fill {fill}); upstream domino: {upstream}"
    );
}

#[test]
fn local_admission_admit_and_locate() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let req = IdleRequest {
                session: SessionId::parse("s1").expect("session id parses"),
                turn: TurnId::new(),
            };
            let lock = admission.admit(req).await.expect("local admission grants the claim");
            assert_eq!(lock.holder_view().locality(), Locality::Here);
            assert_eq!(lock.lease_state(), LeaseState::Live);

            let session = SessionId::parse("s1").expect("session id parses");
            let located = admission
                .locate_holder(&session)
                .await
                .expect("locate holder reads the local arbiter");
            assert!(located.is_some());
            let view = located.unwrap();
            assert_eq!(view.locality(), Locality::Here);
            assert_eq!(view.pod().as_ref(), "pod-0");
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "local_admission_admit_and_locate",
        "LocalAdmission::admit",
        "u5",
    );
}

#[test]
fn local_admission_same_session_rejected() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let session = SessionId::parse("s1").expect("session id parses");
            let req1 = IdleRequest {
                session: session.clone(),
                turn: TurnId::new(),
            };
            let _lock1 = admission.admit(req1).await.expect("first admit grants");
            let req2 = IdleRequest {
                session,
                turn: TurnId::new(),
            };
            let second = admission.admit(req2).await;
            assert!(second.is_err());
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "local_admission_same_session_rejected",
        "LocalAdmission::admit",
        "u5",
    );
}
