//! Conformance and contract tests for the file-backed approval store
//! (`AURA_SESSION_STORE=file`): the shared backend-agnostic battery plus the
//! §2.5 contract points specific to durable, retention-until-remove storage —
//! expiry refusal, retention past the decision, the ticket's move into the
//! decision file, owner-scoped cancel of undecided tickets, and survival of a
//! process restart. The same battery runs against the memory backend to pin
//! the uniform contract. No Docker: every test gets its own tempdir.

mod common;

use std::sync::Arc;
use std::time::Duration;

use aura::hitl::{ApprovalDecision, ResolveError};
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

// ---------------------------------------------------------------------------
// §2.5 contract: layout, expiry, retention, the move, owner-scoped cancel
// ---------------------------------------------------------------------------

/// The constructor creates the layout's two directories.
#[test]
fn open_creates_the_ticket_and_decision_directories() {
    let dir = tempfile::tempdir().unwrap();
    FileApprovalStore::open(dir.path()).unwrap();
    assert!(dir.path().join("tickets").is_dir());
    assert!(dir.path().join("decisions").is_dir());
}

/// §2.5: `resolve` refuses past the ticket's `expires_at`, uniformly with an
/// unknown id; nothing is decided, and the expired ticket stays readable
/// through `get` until `remove`.
#[tokio::test]
async fn expired_resolve_is_not_found_and_ticket_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let mut parked = make_parked("req-expired", Duration::from_secs(60));
    parked.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();

    assert_eq!(
        store.resolve(&id, ApprovalDecision::Approved).await,
        Err(ResolveError::NotFound)
    );
    assert_eq!(store.decision(&id).await.unwrap(), None);
    let restored = store
        .get(&id)
        .await
        .unwrap()
        .expect("expired ticket retained until remove");
    assert_eq!(restored.request.decision_id, id);
}

/// §2.5: retention is until remove — `get` returns the ticket before and
/// after the decision, `decision` returns the record, and only `remove`
/// clears both.
#[tokio::test]
async fn ticket_and_decision_are_retained_until_remove() {
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
        .resolve(&id, ApprovalDecision::Approved)
        .await
        .unwrap();

    assert_eq!(
        ParkedApprovalRecord::from(
            &store
                .get(&id)
                .await
                .unwrap()
                .expect("ticket survives its decision")
        ),
        expected
    );
    assert_eq!(
        store.decision(&id).await.unwrap(),
        Some(ApprovalDecision::Approved)
    );

    store.remove(&id).await.unwrap();
    assert!(store.get(&id).await.unwrap().is_none());
    assert_eq!(store.decision(&id).await.unwrap(), None);
    assert_eq!(
        store.resolve(&id, ApprovalDecision::Approved).await,
        Err(ResolveError::NotFound)
    );
}

/// §2.5: `resolve` moves the ticket into the decision file rather than
/// deleting it — on disk the ticket file is gone and the decision file
/// carries both the ticket record and the decision.
#[tokio::test]
async fn resolve_moves_the_ticket_into_the_decision_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-move", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();

    store
        .resolve(&id, ApprovalDecision::Approved)
        .await
        .unwrap();

    assert!(
        !dir.path()
            .join("tickets")
            .join(format!("{id}.json"))
            .exists()
    );
    let decision_path = dir.path().join("decisions").join(format!("{id}.json"));
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(decision_path).unwrap()).unwrap();
    assert_eq!(on_disk["ticket"]["decision_id"], id.to_string());
    assert_eq!(on_disk["ticket"]["request_id"], "req-move");
    assert_eq!(on_disk["decision"]["approved"], true);
    assert_eq!(on_disk["decision"]["reason"], serde_json::Value::Null);
}

/// §2.5: `cancel_request` removes undecided tickets by owner id; a decided
/// ticket of the same owner and an undecided ticket of another owner survive.
#[tokio::test]
async fn cancel_request_removes_only_undecided_matching_tickets() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let undecided = make_parked("req-owner", Duration::from_secs(60));
    let undecided_id = undecided.request.decision_id;
    store.register(undecided).await.unwrap();
    let decided = make_parked("req-owner", Duration::from_secs(60));
    let decided_id = decided.request.decision_id;
    store.register(decided).await.unwrap();
    store
        .resolve(&decided_id, ApprovalDecision::Approved)
        .await
        .unwrap();
    let other = make_parked("req-other", Duration::from_secs(60));
    let other_id = other.request.decision_id;
    store.register(other).await.unwrap();

    store.cancel_request("req-owner").await.unwrap();

    assert!(store.get(&undecided_id).await.unwrap().is_none());
    assert!(
        store.get(&decided_id).await.unwrap().is_some(),
        "decided ticket is retained until remove"
    );
    assert_eq!(
        store.decision(&decided_id).await.unwrap(),
        Some(ApprovalDecision::Approved)
    );
    assert!(store.get(&other_id).await.unwrap().is_some());
}

/// A read-only `tickets/` directory must not fail `resolve`: the decision
/// write and its sync are the commit, and the ticket removal past them is
/// best-effort — the stale ticket remains, `get` still returns the record,
/// and the claim still holds against a repeat resolve.
#[cfg(unix)]
#[tokio::test]
async fn resolve_succeeds_when_the_ticket_file_cannot_be_removed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-readonly", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();

    let tickets = dir.path().join("tickets");
    std::fs::set_permissions(&tickets, std::fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = Restore(&tickets);

    // Root bypasses directory permission bits; the fault this test pins
    // cannot be established there, so skip rather than pass vacuously.
    let probe = tickets.join(".write-probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipping: process bypasses directory permissions (running as root?)");
        return;
    }

    store
        .resolve(&id, ApprovalDecision::Approved)
        .await
        .expect("resolve commits without the ticket removal");
    assert_eq!(
        store.decision(&id).await.unwrap(),
        Some(ApprovalDecision::Approved)
    );
    let restored = store
        .get(&id)
        .await
        .unwrap()
        .expect("ticket record survives the failed removal");
    assert_eq!(restored.request.decision_id, id);
    assert_eq!(
        store.resolve(&id, ApprovalDecision::Approved).await,
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
        .resolve(&id, ApprovalDecision::Approved)
        .await
        .expect("resolve after reopen");
    assert_eq!(
        reopened.decision(&id).await.unwrap(),
        Some(ApprovalDecision::Approved)
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
    let tickets = dir.path().join("tickets");
    std::fs::set_permissions(&tickets, std::fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = Restore(&tickets);

    let probe = tickets.join(".write-probe");
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

/// Ping is the file backend's readiness signal: it must report an
/// unwritable directory, and recover when writability returns.
#[cfg(unix)]
#[tokio::test]
async fn ping_reports_an_unwritable_store_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    store.probe_writable().await.expect("healthy store pings");

    let tickets = dir.path().join("tickets");
    std::fs::set_permissions(&tickets, std::fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = Restore(&tickets);

    let probe = tickets.join(".write-probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipping: process bypasses directory permissions (running as root?)");
        return;
    }

    store
        .probe_writable()
        .await
        .expect_err("ping must report the unwritable directory");
    drop(store);

    std::fs::set_permissions(&tickets, std::fs::Permissions::from_mode(0o755)).unwrap();
    let restored = FileApprovalStore::open(dir.path()).unwrap();
    restored.probe_writable().await.expect("ping recovers");
}
