// Frames for DESIGN.md sections:
// - write_artifact_publishes: Type-to-business-rule map: ActiveTurn::write_artifact
// - write_artifact_rejects_duplicate: Type-to-business-rule map: ArtifactWriteError::Duplicate
// - write_artifact_rejects_wrong_epoch: Type-to-business-rule map: ArtifactWriteError::WrongEpoch
// - read_artifact_not_found_after_exhaustion: Type-to-business-rule map: FencedRun::read_artifact
// - scratchpad_write_read_same_turn: Type-to-business-rule map: ScratchpadName / ActiveTurn scratchpad I/O

use std::panic::{AssertUnwindSafe, catch_unwind};

use session_guard::{
    ArtifactPath, IdleRequest, PodId, ScratchpadName, SessionId, TurnId, build_admission,
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
fn write_artifact_publishes() {
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

            let path = ArtifactPath::parse("e1/artifact.txt").expect("path parses");
            let entry = active
                .write_artifact(path.clone(), b"hello")
                .await
                .expect("write succeeds");
            assert_eq!(entry.bytes, 5);
            let file_path = root
                .path()
                .join(session.as_ref())
                .join(<ArtifactPath as AsRef<std::path::Path>>::as_ref(&path));
            assert!(tokio::fs::try_exists(&file_path).await.unwrap_or(false));
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "write_artifact_publishes",
        "ActiveTurn::write_artifact",
        "u6",
    );
}

#[test]
fn write_artifact_rejects_duplicate() {
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

            let path = ArtifactPath::parse("e1/artifact.txt").expect("path parses");
            let first = active.write_artifact(path.clone(), b"first");
            let second = active.write_artifact(path, b"second");
            let (r1, r2) = tokio::join!(first, second);
            r1.expect("first write succeeds");
            let err = r2.expect_err("second concurrent write of same path is rejected");
            match err {
                session_guard::ArtifactWriteError::Duplicate(_) => {}
                other => panic!("expected Duplicate, got {other:?}"),
            }
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "write_artifact_rejects_duplicate",
        "ActiveTurn::write_artifact",
        "u6",
    );
}

#[test]
fn write_artifact_rejects_wrong_epoch() {
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

            let path = ArtifactPath::parse("e2/artifact.txt").expect("path parses");
            let err = active
                .write_artifact(path, b"data")
                .await
                .expect_err("write outside claiming epoch is rejected");
            match err {
                session_guard::ArtifactWriteError::WrongEpoch { .. } => {}
                other => panic!("expected WrongEpoch, got {other:?}"),
            }
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "write_artifact_rejects_wrong_epoch",
        "ActiveTurn::write_artifact",
        "u6",
    );
}

#[test]
fn read_artifact_not_found_after_exhaustion() {
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

            let path = ArtifactPath::parse("e1/missing.txt").expect("path parses");
            let err = active
                .read_artifact(&path)
                .await
                .expect_err("missing artifact is rejected after window + repair exhaust");
            match err {
                session_guard::ReadError::Miss(session_guard::ReadMiss::NotFound(_)) => {}
                other => panic!("expected NotFound miss, got {other:?}"),
            }
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "read_artifact_not_found_after_exhaustion",
        "FencedRun::read_artifact",
        "u6",
    );
}

#[test]
fn scratchpad_write_read_same_turn() {
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

            let name = ScratchpadName::parse("scratch").expect("scratchpad name parses");
            active
                .write_scratchpad(&name, b"scratch bytes")
                .await
                .expect("scratchpad write succeeds");
            let bytes = active
                .read_scratchpad(&name)
                .await
                .expect("scratchpad read returns same-turn bytes");
            assert_eq!(bytes, b"scratch bytes");
        });
    }));
    expect_todo_panic_with_domino(
        result,
        "scratchpad_write_read_same_turn",
        "ActiveTurn scratchpad I/O",
        "u6",
    );
}
