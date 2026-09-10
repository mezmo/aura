//! Conformance and contract tests for the file-backed approval store
//! (`AURA_SESSION_STORE=file`): the shared backend-agnostic battery plus the
//! §2.5 contract points specific to durable, retention-until-remove storage —
//! expiry refusal, retention past the decision, the approval's move into the
//! decision file, owner-scoped cancel of undecided approvals, and survival of a
//! process restart. The same battery runs against the memory backend to pin
//! the uniform contract. No Docker: every test gets its own tempdir.

mod common;

use std::sync::Arc;
use std::time::Duration;

use aura::hitl::{ApprovalDecision, ResolveError, ResolvedDecision};
use aura::session_store::{
    ApprovalStore, FileApprovalStore, InMemoryApprovalStore, ParkedApprovalRecord,
    SessionStoreError,
};

use common::make_parked;

/// Two handles to one file store at `dir`'s root: the single-writing-process
/// deployment shape.
fn file_pair(dir: &tempfile::TempDir) -> (Arc<dyn ApprovalStore>, Arc<dyn ApprovalStore>) {
    let store: Arc<dyn ApprovalStore> = Arc::new(FileApprovalStore::open(dir.path()).unwrap());
    (Arc::clone(&store), store)
}

/// Two handles to one in-memory store: the same shape for the default
/// backend.
fn memory_pair() -> (Arc<dyn ApprovalStore>, Arc<dyn ApprovalStore>) {
    let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
    (Arc::clone(&store), store)
}

// ---------------------------------------------------------------------------
// Shared battery vs the file backend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_battery_register_get_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, instance_b) = file_pair(&dir);
    common::register_get_roundtrip(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn file_battery_resolve_is_at_most_once() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, instance_b) = file_pair(&dir);
    common::resolve_is_at_most_once(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn file_battery_concurrent_resolves_have_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, instance_b) = file_pair(&dir);
    common::concurrent_resolves_have_exactly_one_winner(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn file_battery_resolve_records_readable_decision() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, instance_b) = file_pair(&dir);
    common::resolve_records_readable_decision(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn file_battery_resolve_records_identity_with_the_decision() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, instance_b) = file_pair(&dir);
    common::resolve_records_identity_with_the_decision(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn memory_battery_resolve_records_identity_with_the_decision() {
    let instance_a: std::sync::Arc<dyn aura::session_store::ApprovalStore> =
        std::sync::Arc::new(aura::session_store::InMemoryApprovalStore::new());
    let instance_b = std::sync::Arc::clone(&instance_a);
    common::resolve_records_identity_with_the_decision(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn file_battery_remove_makes_resolve_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, _) = file_pair(&dir);
    common::remove_makes_resolve_not_found(&instance_a).await;
}

#[tokio::test]
async fn file_battery_cancel_request_removes_only_matching() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, _) = file_pair(&dir);
    common::cancel_request_removes_only_matching(&instance_a).await;
}

#[tokio::test]
async fn file_battery_list_pending_returns_only_live_undecided() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, instance_b) = file_pair(&dir);
    common::list_pending_returns_only_live_undecided(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn file_battery_list_pending_empty_store_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, _) = file_pair(&dir);
    common::list_pending_empty_store_returns_empty(&instance_a).await;
}

#[tokio::test]
async fn file_battery_list_pending_excludes_expired() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_a, instance_b) = file_pair(&dir);
    common::list_pending_excludes_expired(&instance_a, &instance_b).await;
}

// ---------------------------------------------------------------------------
// Shared battery vs the memory backend (uniform contract, no Docker)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_battery_register_get_roundtrip() {
    let (instance_a, instance_b) = memory_pair();
    common::register_get_roundtrip(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn memory_battery_resolve_is_at_most_once() {
    let (instance_a, instance_b) = memory_pair();
    common::resolve_is_at_most_once(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn memory_battery_concurrent_resolves_have_exactly_one_winner() {
    let (instance_a, instance_b) = memory_pair();
    common::concurrent_resolves_have_exactly_one_winner(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn memory_battery_resolve_records_readable_decision() {
    let (instance_a, instance_b) = memory_pair();
    common::resolve_records_readable_decision(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn memory_battery_remove_makes_resolve_not_found() {
    let (instance_a, _) = memory_pair();
    common::remove_makes_resolve_not_found(&instance_a).await;
}

#[tokio::test]
async fn memory_battery_cancel_request_removes_only_matching() {
    let (instance_a, _) = memory_pair();
    common::cancel_request_removes_only_matching(&instance_a).await;
}

#[tokio::test]
async fn memory_battery_list_pending_returns_only_live_undecided() {
    let (instance_a, instance_b) = memory_pair();
    common::list_pending_returns_only_live_undecided(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn memory_battery_list_pending_empty_store_returns_empty() {
    let (instance_a, _) = memory_pair();
    common::list_pending_empty_store_returns_empty(&instance_a).await;
}

#[tokio::test]
async fn memory_battery_list_pending_excludes_expired() {
    let (instance_a, instance_b) = memory_pair();
    common::list_pending_excludes_expired(&instance_a, &instance_b).await;
}

// ---------------------------------------------------------------------------
// §2.5 contract: layout, expiry, retention, the move, owner-scoped cancel
// ---------------------------------------------------------------------------

/// The constructor creates the layout's two directories.
#[test]
fn open_creates_the_approval_and_decision_directories() {
    let dir = tempfile::tempdir().unwrap();
    FileApprovalStore::open(dir.path()).unwrap();
    assert!(dir.path().join("approvals").is_dir());
    assert!(dir.path().join("decisions").is_dir());
}

/// §2.5: `resolve` refuses past the approval's `expires_at`, uniformly with an
/// unknown id; nothing is decided, and the expired approval stays readable
/// through `get` until `remove`.
#[tokio::test]
async fn expired_resolve_is_not_found_and_approval_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let mut parked = make_parked("req-expired", Duration::from_secs(60));
    parked.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();

    assert_eq!(
        store.resolve(&id, ApprovalDecision::Approved.into()).await,
        Err(ResolveError::NotFound)
    );
    assert_eq!(store.decision(&id).await.unwrap(), None);
    let restored = store
        .get(&id)
        .await
        .unwrap()
        .expect("expired approval retained until remove");
    assert_eq!(restored.request.decision_id, id);
}

/// §2.5: retention is until remove — `get` returns the approval before and
/// after the decision, `decision` returns the record, and only `remove`
/// clears both.
#[tokio::test]
async fn approval_and_decision_are_retained_until_remove() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-retain", Duration::from_secs(60));
    let id = parked.request.decision_id;
    let expected = ParkedApprovalRecord::from(&parked);
    store.register(parked).await.unwrap();

    assert_eq!(
        ParkedApprovalRecord::from(&store.get(&id).await.unwrap().unwrap()),
        expected
    );

    store
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    assert_eq!(
        ParkedApprovalRecord::from(
            &store
                .get(&id)
                .await
                .unwrap()
                .expect("approval survives its decision")
        ),
        expected
    );
    assert_eq!(
        store.decision(&id).await.unwrap(),
        Some(ResolvedDecision::from(ApprovalDecision::Approved))
    );

    store.remove(&id).await.unwrap();
    assert!(store.get(&id).await.unwrap().is_none());
    assert_eq!(store.decision(&id).await.unwrap(), None);
    assert_eq!(
        store.resolve(&id, ApprovalDecision::Approved.into()).await,
        Err(ResolveError::NotFound)
    );
}

/// §2.5: `resolve` moves the approval into the decision file rather than
/// deleting it — on disk the approval file is gone and the decision file
/// carries the approval record, minus its egress headers, and the decision.
#[tokio::test]
async fn resolve_moves_the_approval_into_the_decision_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let mut parked = make_parked("req-move", Duration::from_secs(60));
    let mut egress = reqwest::header::HeaderMap::new();
    egress.insert("x-tenant-egress", "tenant-secret".parse().unwrap());
    parked.egress_headers = Some(egress);
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();

    store
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    assert!(
        !dir.path()
            .join("approvals")
            .join(format!("{id}.json"))
            .exists()
    );
    let decision_path = dir.path().join("decisions").join(format!("{id}.json"));
    let raw = std::fs::read_to_string(decision_path).unwrap();
    let on_disk: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(on_disk["approval"]["decision_id"], id.to_string());
    assert_eq!(on_disk["approval"]["request_id"], "req-move");
    assert_eq!(on_disk["decision"]["approved"], true);
    assert_eq!(on_disk["decision"]["reason"], serde_json::Value::Null);
    assert!(
        !raw.contains("tenant-secret"),
        "egress credential survived resolve"
    );
}

/// §2.5: `cancel_request` removes undecided approvals by owner id and
/// returns exactly the cleared set; a decided approval of the same owner and
/// an undecided approval of another owner survive.
#[tokio::test]
async fn cancel_request_removes_only_undecided_matching_approvals() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let undecided = make_parked("req-owner", Duration::from_secs(60));
    let undecided_id = undecided.request.decision_id;
    store.register(undecided).await.unwrap();
    let decided = make_parked("req-owner", Duration::from_secs(60));
    let decided_id = decided.request.decision_id;
    store.register(decided).await.unwrap();
    store
        .resolve(&decided_id, ApprovalDecision::Approved.into())
        .await
        .unwrap();
    let other = make_parked("req-other", Duration::from_secs(60));
    let other_id = other.request.decision_id;
    store.register(other).await.unwrap();

    let cleared = store.cancel_request("req-owner").await.unwrap();

    assert_eq!(cleared.len(), 1, "only the undecided ticket is cleared");
    assert_eq!(cleared[0].request.decision_id, undecided_id);
    assert!(store.get(&undecided_id).await.unwrap().is_none());
    assert!(
        store.get(&decided_id).await.unwrap().is_some(),
        "decided approval is retained until remove"
    );
    assert_eq!(
        store.decision(&decided_id).await.unwrap(),
        Some(ResolvedDecision::from(ApprovalDecision::Approved))
    );
    assert!(store.get(&other_id).await.unwrap().is_some());
}

/// The residue of resolve's best-effort approval unlink — a stale approval
/// file whose decision file exists — is swept by `cancel_request` but absent
/// from the returned set: the recorded decision owns the outcome.
#[tokio::test]
async fn cancel_request_sweeps_a_stale_decided_approval_without_returning_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let undecided = make_parked("req-residue", Duration::from_secs(60));
    let undecided_id = undecided.request.decision_id;
    store.register(undecided).await.unwrap();
    let decided = make_parked("req-residue", Duration::from_secs(60));
    let decided_id = decided.request.decision_id;
    let residue = serde_json::to_vec(&ParkedApprovalRecord::from(&decided)).unwrap();
    store.register(decided).await.unwrap();
    store
        .resolve(&decided_id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    // Resolve's approval unlink failed: put the residue back.
    std::fs::write(
        dir.path()
            .join("approvals")
            .join(format!("{decided_id}.json")),
        residue,
    )
    .unwrap();

    let cleared = store.cancel_request("req-residue").await.unwrap();

    assert_eq!(cleared.len(), 1, "only the undecided ticket is cleared");
    assert_eq!(cleared[0].request.decision_id, undecided_id);
    assert!(
        !dir.path()
            .join("approvals")
            .join(format!("{undecided_id}.json"))
            .exists()
    );
    assert!(
        !dir.path()
            .join("approvals")
            .join(format!("{decided_id}.json"))
            .exists(),
        "the stale residue is swept too"
    );
    assert_eq!(
        store.decision(&decided_id).await.unwrap(),
        Some(ResolvedDecision::from(ApprovalDecision::Approved)),
        "the recorded decision is retained"
    );
}

/// A corrupt record in `approvals/` is warn-and-skipped: `cancel_request`
/// still returns and removes the decodable files, leaving the corrupt file
/// in place.
#[tokio::test]
async fn cancel_request_skips_an_undecodable_approval_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-corrupt", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();
    let corrupt = dir.path().join("approvals").join("corrupt.json");
    std::fs::write(&corrupt, b"not json").unwrap();

    let cleared = store.cancel_request("req-corrupt").await.unwrap();

    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].request.decision_id, id);
    assert!(
        !dir.path()
            .join("approvals")
            .join(format!("{id}.json"))
            .exists()
    );
    assert!(corrupt.exists(), "the undecodable file is left in place");
}

#[tokio::test]
async fn list_pending_skips_an_undecodable_approval_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-poll-corrupt", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();
    let corrupt = dir.path().join("approvals").join("corrupt.json");
    std::fs::write(&corrupt, b"not json").unwrap();

    let pending = store.list_pending().await.unwrap();

    let ids: Vec<_> = pending.iter().map(|p| p.request.decision_id).collect();
    assert_eq!(ids, [id], "the corrupt file must not fail the scan");
    assert!(corrupt.exists(), "the undecodable file is left in place");
}

/// The residue of resolve's best-effort approval unlink — a stale approval
/// file whose decision file exists — never re-enters the reconciler's scan:
/// the recorded decision owns the outcome.
#[tokio::test]
async fn list_pending_skips_a_stale_decided_approval() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let live = make_parked("req-poll-live", Duration::from_secs(60));
    let live_id = live.request.decision_id;
    store.register(live).await.unwrap();
    let decided = make_parked("req-poll-residue", Duration::from_secs(60));
    let decided_id = decided.request.decision_id;
    let residue = serde_json::to_vec(&ParkedApprovalRecord::from(&decided)).unwrap();
    store.register(decided).await.unwrap();
    store
        .resolve(&decided_id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    // Resolve's approval unlink failed: put the residue back.
    std::fs::write(
        dir.path()
            .join("approvals")
            .join(format!("{decided_id}.json")),
        residue,
    )
    .unwrap();

    let pending = store.list_pending().await.unwrap();

    let ids: Vec<_> = pending.iter().map(|p| p.request.decision_id).collect();
    assert_eq!(ids, [live_id], "the decided residue must not be listed");
}

/// A read-only `approvals/` directory must not fail `resolve`: the decision
/// write and its sync are the commit, and the approval removal past them is
/// best-effort — the stale approval remains, `get` still returns the record,
/// and the claim still holds against a repeat resolve.
#[cfg(unix)]
#[tokio::test]
async fn resolve_succeeds_when_the_approval_file_cannot_be_removed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-readonly", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();

    let approvals = dir.path().join("approvals");
    std::fs::set_permissions(&approvals, std::fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = Restore(&approvals);

    // Root bypasses directory permission bits; the fault this test pins
    // cannot be established there, so skip rather than pass vacuously.
    let probe = approvals.join(".write-probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipping: process bypasses directory permissions (running as root?)");
        return;
    }

    store
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .expect("resolve commits without the approval removal");
    assert_eq!(
        store.decision(&id).await.unwrap(),
        Some(ResolvedDecision::from(ApprovalDecision::Approved))
    );
    let restored = store
        .get(&id)
        .await
        .unwrap()
        .expect("approval record survives the failed removal");
    assert_eq!(restored.request.decision_id, id);
    assert_eq!(
        store.resolve(&id, ApprovalDecision::Approved.into()).await,
        Err(ResolveError::NotFound)
    );
}

// ---------------------------------------------------------------------------
// Restart durability
// ---------------------------------------------------------------------------

/// The durability boundary is a process restart: a store reopened at the
/// same path resolves and reads back what the previous instance parked.
#[tokio::test]
async fn state_survives_reopening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let store = FileApprovalStore::open(dir.path()).unwrap();
        let parked = make_parked("req-restart", Duration::from_secs(60));
        let id = parked.request.decision_id;
        store.register(parked).await.unwrap();
        id
    };

    let reopened = FileApprovalStore::open(dir.path()).unwrap();
    reopened
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .expect("resolve after reopen");
    assert_eq!(
        reopened.decision(&id).await.unwrap(),
        Some(ResolvedDecision::from(ApprovalDecision::Approved))
    );
}

/// Restore write permission on drop, so a failed assertion cannot leave
/// the tempdir undeletable.
#[cfg(unix)]
struct Restore<'a>(&'a std::path::Path);
#[cfg(unix)]
impl Drop for Restore<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// A read-only store directory must fail `open`, not the first approval:
/// readiness keys on construction and ping, so an unwritable path rejects
/// the server at startup.
#[cfg(unix)]
#[tokio::test]
async fn open_fails_when_a_store_directory_is_not_writable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    drop(FileApprovalStore::open(dir.path()).unwrap());
    let approvals = dir.path().join("approvals");
    std::fs::set_permissions(&approvals, std::fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = Restore(&approvals);

    let probe = approvals.join(".write-probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipping: process bypasses directory permissions (running as root?)");
        return;
    }

    let err = match FileApprovalStore::open(dir.path()) {
        Ok(_) => panic!("open must refuse an unwritable store directory"),
        Err(err) => err,
    };
    assert!(
        matches!(err, SessionStoreError::Connect { .. }),
        "expected Connect, got {err:?}"
    );
}

/// Ping is the file backend's readiness signal: it must report either
/// store directory becoming unwritable, and recover when writability
/// returns.
#[cfg(unix)]
#[tokio::test]
async fn ping_reports_an_unwritable_store_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    store.probe_writable().await.expect("healthy store pings");

    for name in ["approvals", "decisions"] {
        let locked = dir.path().join(name);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let restore = Restore(&locked);

        let probe = locked.join(".write-probe");
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            eprintln!("skipping: process bypasses directory permissions (running as root?)");
            return;
        }

        store
            .probe_writable()
            .await
            .expect_err("ping must report the unwritable directory");
        drop(restore);
        store.probe_writable().await.expect("ping recovers");
    }
}
