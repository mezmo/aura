// Frames for DESIGN.md sections:
// - held_lock_abort_reports_cleanup: Type-to-business-rule map: HeldLock::abort
// - active_turn_abort_reports_cleanup: Type-to-business-rule map: ActiveTurn::abort
// - barrier_payload_step_first_then_authorizes: Type-to-business-rule map: CommittingTurn::barrier
// - barrier_failed_payload_quarantines: Type-to-business-rule map: BarrierError::CommitFailed
// - barrier_park_authorizes: Type-to-business-rule map: CommittingTurn::barrier

use std::panic::{AssertUnwindSafe, catch_unwind};

use session_guard::{CommitKind, IdleRequest, PodId, SessionId, TurnId, build_admission};

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
fn held_lock_abort_reports_cleanup() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = session_guard::AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let req = IdleRequest {
                session: SessionId::parse("s1").expect("session id parses"),
                turn: TurnId::new(),
            };
            let lock = admission.admit(req).await.expect("local admission grants");
            let outcome = lock.abort().await;
            assert!(outcome.quarantine.is_ok());
            assert!(outcome.release.is_ok());
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "held_lock_abort_reports_cleanup",
        "HeldLock::abort",
        "u7",
    );
}

#[test]
fn active_turn_abort_reports_cleanup() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = session_guard::AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let session = SessionId::parse("s1").expect("session id parses");
            tokio::fs::create_dir_all(root.path().join(session.as_ref()))
                .await
                .expect("session root exists for create_run");
            let req = IdleRequest {
                session: session.clone(),
                turn: TurnId::new(),
            };
            let lock = admission.admit(req).await.expect("local admission grants");
            let run = lock.create_run().await.expect("run dir created");
            let active = run.activate();
            let outcome = active.abort().await;
            assert!(outcome.quarantine.is_ok());
            assert!(outcome.release.is_ok());
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "active_turn_abort_reports_cleanup",
        "ActiveTurn::abort",
        "u7",
    );
}

#[test]
fn barrier_payload_step_first_then_authorizes() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = session_guard::AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let session = SessionId::parse("s1").expect("session id parses");
            tokio::fs::create_dir_all(root.path().join(session.as_ref()))
                .await
                .expect("session root exists for create_run");
            let req = IdleRequest {
                session: session.clone(),
                turn: TurnId::new(),
            };
            let lock = admission.admit(req).await.expect("local admission grants");
            let run = lock.create_run().await.expect("run dir created");
            let active = run.activate();
            let committing = active.complete(CommitKind::Success);
            let response = committing
                .barrier(|_ctx| async move { Ok::<_, &str>("payload") })
                .await
                .expect("local barrier authorizes the payload after the commit step");
            let (payload, _session, _turn, _epoch, _holder) = response.into_parts();
            assert_eq!(payload, "payload");
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "barrier_payload_step_first_then_authorizes",
        "CommittingTurn::barrier",
        "u7",
    );
}

#[test]
fn barrier_failed_payload_quarantines() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = session_guard::AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let session = SessionId::parse("s1").expect("session id parses");
            tokio::fs::create_dir_all(root.path().join(session.as_ref()))
                .await
                .expect("session root exists for create_run");
            let req = IdleRequest {
                session: session.clone(),
                turn: TurnId::new(),
            };
            let lock = admission.admit(req).await.expect("local admission grants");
            let run = lock.create_run().await.expect("run dir created");
            let active = run.activate();
            let committing = active.complete(CommitKind::Success);
            let err = committing
                .barrier(|_ctx| async move { Err::<&str, &str>("step failed") })
                .await
                .expect_err("failed payload step aborts the commit");
            match err {
                session_guard::BarrierError::CommitFailed { error, cleanup } => {
                    assert_eq!(error, "step failed");
                    assert!(cleanup.quarantine.is_ok());
                    assert!(cleanup.release.is_ok());
                }
                other => panic!("expected CommitFailed, got {other:?}"),
            }
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "barrier_failed_payload_quarantines",
        "CommittingTurn::barrier",
        "u7",
    );
}

#[test]
fn barrier_park_authorizes() {
    assert_clean_admission_env();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let pod = PodId::parse("pod-0").expect("pod id parses");
            let env = session_guard::AdmissionEnv::from_env().expect("off config parses");
            let root = tempfile::tempdir().expect("temp root");
            let admission = build_admission(&env, root.path().to_path_buf(), pod)
                .expect("factory dispatches off mode");
            let session = SessionId::parse("s1").expect("session id parses");
            tokio::fs::create_dir_all(root.path().join(session.as_ref()))
                .await
                .expect("session root exists for create_run");
            let req = IdleRequest {
                session: session.clone(),
                turn: TurnId::new(),
            };
            let lock = admission.admit(req).await.expect("local admission grants");
            let run = lock.create_run().await.expect("run dir created");
            let active = run.activate();
            let committing = active.park();
            let response = committing
                .barrier(|_ctx| async move { Ok::<_, &str>("parked") })
                .await
                .expect("local park barrier authorizes");
            let (payload, _session, _turn, _epoch, _holder) = response.into_parts();
            assert_eq!(payload, "parked");
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "barrier_park_authorizes",
        "CommittingTurn::barrier",
        "u7",
    );
}
