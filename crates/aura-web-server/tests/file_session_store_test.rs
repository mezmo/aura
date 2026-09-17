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

use aura::hitl::{
    AddressedApproval, ApprovalAuthority, ApprovalDecision, ApprovalRead, DecisionId, ResolveError,
    ResolvedDecision,
};
use aura::session_store::{
    ApprovalStore, FileApprovalStore, InMemoryApprovalStore, ParkedApprovalRecord,
    RetainedApproval, SessionStoreError,
};

use common::make_parked;

/// The file names in `dir`, sorted: the scan's no-unlink side-effect check
/// compares these sets around the call.
fn dir_listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

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
        store
            .resolve(
                &id,
                ApprovalAuthority::Conversational,
                ApprovalDecision::Approved.into()
            )
            .await,
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
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
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
        store
            .resolve(
                &id,
                ApprovalAuthority::Conversational,
                ApprovalDecision::Approved.into()
            )
            .await,
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
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
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

/// An expired undecided approval leaves the store on the scan that finds
/// it, so no credential outlives the decision window.
#[tokio::test]
async fn list_pending_unlinks_expired_approval_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let mut expired = make_parked("req-expired", Duration::from_secs(60));
    expired.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    let id = expired.request.decision_id;
    store.register(expired).await.unwrap();

    assert!(store.list_pending().await.unwrap().is_empty());
    assert!(
        !dir.path()
            .join("approvals")
            .join(format!("{id}.json"))
            .exists(),
        "the expired approval file was unlinked"
    );
}

/// Rows hold credentials, so the store's directories and files are
/// readable by the owner only.
#[cfg(unix)]
#[tokio::test]
async fn store_directories_and_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-mode", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();
    let mode =
        |path: std::path::PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    assert_eq!(mode(dir.path().join("approvals")), 0o700);
    assert_eq!(mode(dir.path().join("decisions")), 0o700);
    assert_eq!(
        mode(dir.path().join("approvals").join(format!("{id}.json"))),
        0o600
    );

    store
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
        .await
        .unwrap();
    assert_eq!(
        mode(dir.path().join("decisions").join(format!("{id}.json"))),
        0o600
    );
}

/// An approval file left behind after its decision was written (the unlink
/// in `resolve` is best-effort) is neither returned nor kept: the pending
/// scan removes it, so the row's credentials do not outlive the decision.
#[tokio::test]
async fn list_pending_removes_an_approval_file_that_already_has_a_decision() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-residue", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();
    let approval_path = dir.path().join("approvals").join(format!("{id}.json"));
    let approval_bytes = std::fs::read(&approval_path).unwrap();

    store
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
        .await
        .unwrap();
    assert!(!approval_path.exists(), "resolve unlinks the approval file");
    std::fs::write(&approval_path, &approval_bytes).unwrap();

    let pending = store.list_pending().await.unwrap();

    assert_eq!(pending.len(), 0, "a decided id is never pending");
    assert!(
        !approval_path.exists(),
        "the scan removes the decided approval residue"
    );
    assert!(
        dir.path()
            .join("decisions")
            .join(format!("{id}.json"))
            .exists(),
        "the decision record is untouched"
    );
}

/// A decision file that does not decode (a write interrupted before its
/// sync) marks the id as claimed but not recoverable from that file: the
/// scan neither returns the row nor removes the approval file, which stays
/// the only intact record and still answers `get`.
#[tokio::test]
async fn list_pending_keeps_the_approval_file_behind_an_incomplete_decision() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileApprovalStore::open(dir.path()).unwrap();
    let parked = make_parked("req-torn", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();
    let approval_path = dir.path().join("approvals").join(format!("{id}.json"));
    let decision_path = dir.path().join("decisions").join(format!("{id}.json"));
    std::fs::write(&decision_path, b"{\"approval\":").unwrap();

    let pending = store.list_pending().await.unwrap();

    assert_eq!(pending.len(), 0, "a claimed id is never pending");
    assert!(approval_path.exists(), "the intact approval file is kept");
    let got = store
        .get(&id)
        .await
        .unwrap()
        .expect("the approval still reads");
    assert_eq!(got.request.decision_id, id);
    assert!(
        matches!(
            store.decision(&id).await,
            Err(SessionStoreError::Decode { .. })
        ),
        "the torn decision file reads as a decode error"
    );
    assert_eq!(
        store
            .resolve(
                &id,
                ApprovalAuthority::Conversational,
                ApprovalDecision::Approved.into()
            )
            .await,
        Err(ResolveError::NotFound),
        "the torn file still holds the at-most-once claim"
    );
    assert!(
        approval_path.exists(),
        "a refused resolve leaves the approval"
    );

    // Recovery: clearing the torn file returns the row to the pending set
    // and lets a fresh resolve land.
    std::fs::remove_file(&decision_path).unwrap();
    let pending = store.list_pending().await.unwrap();
    assert_eq!(pending.len(), 1, "the row is pending again");
    store
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
        .await
        .expect("a fresh resolve lands");
    assert!(!approval_path.exists(), "resolve unlinks the approval file");
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
        .resolve(
            &decided_id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
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
        .resolve(
            &decided_id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
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
        .resolve(
            &decided_id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
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
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
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
        store
            .resolve(
                &id,
                ApprovalAuthority::Conversational,
                ApprovalDecision::Approved.into()
            )
            .await,
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
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
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

/// A regular file where a store directory belongs must fail `open`, so an
/// unusable path rejects the server at startup rather than the first
/// approval.
#[tokio::test]
async fn open_fails_when_a_store_directory_is_obstructed() {
    let dir = tempfile::tempdir().unwrap();
    drop(FileApprovalStore::open(dir.path()).unwrap());
    let approvals = dir.path().join("approvals");
    std::fs::remove_dir_all(&approvals).unwrap();
    std::fs::write(&approvals, b"obstruction").unwrap();

    let err = match FileApprovalStore::open(dir.path()) {
        Ok(_) => panic!("open must refuse an obstructed store directory"),
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

// ---------------------------------------------------------------------------
// read-or-expire and the retained-evidence scan (E2 RED)
//
// Every test here drives the store through `open_with_clock` with the clock
// pinned to a fixed instant, so deadline and timeout arbitration are
// deterministic: a test names the instant the store must sample, and the
// wall clock never participates.
// ---------------------------------------------------------------------------

/// A store whose clock is pinned to `now`: every operation samples exactly
/// this instant, under the same lock resolve and remove hold.
fn store_pinned_at(
    dir: &tempfile::TempDir,
    now: chrono::DateTime<chrono::Utc>,
) -> FileApprovalStore {
    let clock: Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync> = Arc::new(move || now);
    FileApprovalStore::open_with_clock(dir.path(), clock).unwrap()
}

/// A wrong-authority read answers `Missing` with no mutation: the approval
/// file stays on disk and the row still reads through `get`.
#[tokio::test]
async fn file_read_or_expire_missing_row_reads_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_pinned_at(&dir, chrono::Utc::now());

    assert!(
        matches!(
            store
                .read_or_expire(&DecisionId::generate(), ApprovalAuthority::Conversational)
                .await
                .unwrap(),
            ApprovalRead::Missing
        ),
        "an unknown id must read as Missing"
    );
}

#[tokio::test]
async fn file_read_or_expire_wrong_authority_reads_missing_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_pinned_at(&dir, chrono::Utc::now());
    let parked = make_parked("req-roe-authority", Duration::from_secs(60));
    let id = parked.request.decision_id;
    store.register(parked).await.unwrap();

    assert!(
        matches!(
            store
                .read_or_expire(&id, ApprovalAuthority::WebhookPoll)
                .await
                .unwrap(),
            ApprovalRead::Missing
        ),
        "a wrong-authority read must answer Missing"
    );
    assert!(
        dir.path()
            .join("approvals")
            .join(format!("{id}.json"))
            .exists(),
        "a wrong-authority read must not unlink the approval file"
    );
    assert!(
        store.get(&id).await.unwrap().is_some(),
        "a wrong-authority read must not consume the row"
    );
}

/// A row inside its window reads as pending.
#[tokio::test]
async fn file_read_or_expire_pending_inside_window_is_pending() {
    let dir = tempfile::tempdir().unwrap();
    let parked = make_parked("req-roe-pending", Duration::from_secs(60));
    let id = parked.request.decision_id;
    let expected = ParkedApprovalRecord::from(&parked);
    let store = store_pinned_at(&dir, parked.expires_at - chrono::Duration::seconds(1));
    store.register(parked).await.unwrap();

    match store
        .read_or_expire(&id, ApprovalAuthority::Conversational)
        .await
        .unwrap()
    {
        ApprovalRead::Pending(got) => assert_eq!(ParkedApprovalRecord::from(&got), expected),
        _ => panic!("expected Pending, got another ApprovalRead arm"),
    }
}

/// Strictly past the deadline with no decision, the read expires the row
/// durably: a `TimedOut` decision file carrying the row's own deadline
/// replaces the approval file (resolve's move convention), the addressed
/// answer carries that deadline, and a second read returns the same
/// terminal winner instead of re-expiring.
#[tokio::test]
async fn file_read_or_expire_expired_row_writes_durable_timed_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut parked = make_parked("req-roe-expired", Duration::from_secs(60));
    let mut egress = reqwest::header::HeaderMap::new();
    egress.insert("x-tenant-egress", "tenant-secret".parse().unwrap());
    parked.egress_headers = Some(egress);
    let id = parked.request.decision_id;
    // The durable record carries the row with its egress headers stripped —
    // resolve's move convention — so that is the shape both reads return.
    let mut expected_row = parked.clone();
    expected_row.egress_headers = None;
    let expected = ParkedApprovalRecord::from(&expected_row);
    let deadline = parked.expires_at;
    let store = store_pinned_at(&dir, deadline + chrono::Duration::seconds(1));
    store.register(parked).await.unwrap();

    for _ in 0..2 {
        match store
            .read_or_expire(&id, ApprovalAuthority::Conversational)
            .await
            .unwrap()
        {
            ApprovalRead::Addressed { approval, outcome } => {
                assert_eq!(
                    ParkedApprovalRecord::from(&approval),
                    expected,
                    "the addressed approval is the egress-stripped persisted row"
                );
                assert_eq!(outcome, AddressedApproval::TimedOut { deadline });
            }
            _ => panic!("expected Addressed, got another ApprovalRead arm"),
        }
    }

    assert!(
        !dir.path()
            .join("approvals")
            .join(format!("{id}.json"))
            .exists(),
        "the expired approval file is moved, not left behind"
    );
    let raw = std::fs::read_to_string(dir.path().join("decisions").join(format!("{id}.json")))
        .expect("the durable timeout row is persisted");
    let on_disk: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(on_disk["decision"]["kind"], "timed_out");
    assert!(
        !raw.contains("x-tenant-egress") && !raw.contains("tenant-secret"),
        "egress credentials must not survive into the decision file"
    );
}

/// A decision recorded inside the window wins: the read addresses with the
/// recorded decision and never rewrites the decision file into a timeout.
#[tokio::test]
async fn file_read_or_expire_decided_winner_is_addressed_decided() {
    let dir = tempfile::tempdir().unwrap();
    let parked = make_parked("req-roe-decided", Duration::from_secs(60));
    let id = parked.request.decision_id;
    let expected = ParkedApprovalRecord::from(&parked);
    let store = store_pinned_at(&dir, parked.expires_at - chrono::Duration::seconds(1));
    store.register(parked).await.unwrap();
    store
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
        .await
        .unwrap();
    let decided_raw =
        std::fs::read_to_string(dir.path().join("decisions").join(format!("{id}.json"))).unwrap();

    match store
        .read_or_expire(&id, ApprovalAuthority::Conversational)
        .await
        .unwrap()
    {
        ApprovalRead::Addressed { approval, outcome } => {
            assert_eq!(ParkedApprovalRecord::from(&approval), expected);
            assert_eq!(
                outcome,
                AddressedApproval::Decided(ResolvedDecision::from(ApprovalDecision::Approved))
            );
        }
        _ => panic!("expected Addressed, got another ApprovalRead arm"),
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("decisions").join(format!("{id}.json"))).unwrap(),
        decided_raw,
        "an existing terminal winner is returned unchanged"
    );
}

/// Decode, unknown-id, and I/O failures are errors, never outcomes: a
/// decision file that does not decode must fail the read as
/// `SessionStoreError::Decode`, never as a fabricated `Missing`.
#[tokio::test]
async fn file_read_or_expire_corrupt_decision_file_is_a_decode_error() {
    let dir = tempfile::tempdir().unwrap();
    let parked = make_parked("req-roe-corrupt", Duration::from_secs(60));
    let id = parked.request.decision_id;
    let store = store_pinned_at(&dir, parked.expires_at - chrono::Duration::seconds(1));
    store.register(parked).await.unwrap();
    store
        .resolve(
            &id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
        .await
        .unwrap();
    std::fs::write(
        dir.path().join("decisions").join(format!("{id}.json")),
        b"not json",
    )
    .unwrap();

    match store
        .read_or_expire(&id, ApprovalAuthority::Conversational)
        .await
    {
        Err(SessionStoreError::Decode { .. }) => {}
        _ => panic!("a corrupt decision file must read as a decode error, not another answer"),
    }
}

/// The deadline rule is strictly past: a row sampled exactly at its own
/// `expires_at` is still pending.
#[tokio::test]
async fn file_read_or_expire_deadline_exact_is_still_pending() {
    let dir = tempfile::tempdir().unwrap();
    let parked = make_parked("req-roe-exact", Duration::from_secs(60));
    let id = parked.request.decision_id;
    let store = store_pinned_at(&dir, parked.expires_at);
    store.register(parked).await.unwrap();

    assert!(
        matches!(
            store
                .read_or_expire(&id, ApprovalAuthority::Conversational)
                .await
                .unwrap(),
            ApprovalRead::Pending(_)
        ),
        "an exact-deadline read is still pending"
    );
}

/// The retained scan covers pending AND addressed rows — rows inside and
/// past their window, decided and timed-out — each exactly once, and it
/// unlinks nothing.
#[tokio::test]
async fn file_retained_rows_scans_pending_and_addressed() {
    let dir = tempfile::tempdir().unwrap();
    let expired = make_parked("req-retain-expired", Duration::from_secs(60));
    let expired_id = expired.request.decision_id;
    let expired_deadline = expired.expires_at;
    // The scan samples the pinned clock; the four rows' windows sit either
    // side of it — one pending inside its window, one pending past it, one
    // decided, one timed out through a first read_or_expire.
    let scan_now = expired_deadline + chrono::Duration::seconds(1);
    let mut inside = make_parked("req-retain-inside", Duration::from_secs(60));
    inside.expires_at = scan_now + chrono::Duration::seconds(60);
    let inside_id = inside.request.decision_id;
    let mut past = make_parked("req-retain-past", Duration::from_secs(60));
    past.expires_at = scan_now - chrono::Duration::seconds(1);
    let past_id = past.request.decision_id;
    let decided = make_parked("req-retain-decided", Duration::from_secs(60));
    let decided_id = decided.request.decision_id;

    // Registration and resolve run through a wall-clock store on the same
    // root (their deadlines are open there); only the timed-out row is
    // consumed through the pinned seam.
    let registrar = FileApprovalStore::open(dir.path()).unwrap();
    registrar.register(inside).await.unwrap();
    registrar.register(past).await.unwrap();
    registrar.register(decided).await.unwrap();
    registrar.register(expired).await.unwrap();
    registrar
        .resolve(
            &decided_id,
            ApprovalAuthority::Conversational,
            ApprovalDecision::Approved.into(),
        )
        .await
        .unwrap();

    let store = store_pinned_at(&dir, scan_now);
    assert!(matches!(
        store
            .read_or_expire(&expired_id, ApprovalAuthority::Conversational)
            .await
            .unwrap(),
        ApprovalRead::Addressed { .. }
    ));

    let approvals_before = dir_listing(&dir.path().join("approvals"));
    let decisions_before = dir_listing(&dir.path().join("decisions"));

    let rows = store.retained_rows().await.unwrap();

    assert_eq!(approvals_before, dir_listing(&dir.path().join("approvals")));
    assert_eq!(decisions_before, dir_listing(&dir.path().join("decisions")));
    let mut scanned: Vec<(String, &str)> = Vec::new();
    for row in rows {
        match row {
            RetainedApproval::Pending(parked) => {
                let id = parked.request.decision_id;
                if id == past_id {
                    assert!(
                        parked.expires_at <= scan_now,
                        "the past-window row really is past"
                    );
                }
                if id == inside_id {
                    assert!(
                        parked.expires_at > scan_now,
                        "the inside row really is inside its window"
                    );
                }
                scanned.push((id.to_string(), "pending"));
            }
            RetainedApproval::Addressed { approval, outcome } => match outcome {
                AddressedApproval::Decided(_) => {
                    assert_eq!(approval.request.decision_id, decided_id);
                    scanned.push((decided_id.to_string(), "decided"));
                }
                AddressedApproval::TimedOut { deadline } => {
                    assert_eq!(approval.request.decision_id, expired_id);
                    assert_eq!(deadline, expired_deadline);
                    scanned.push((expired_id.to_string(), "timed_out"));
                }
            },
        }
    }
    scanned.sort();
    let mut expected: Vec<(String, &str)> = vec![
        (inside_id.to_string(), "pending"),
        (past_id.to_string(), "pending"),
        (decided_id.to_string(), "decided"),
        (expired_id.to_string(), "timed_out"),
    ];
    expected.sort();
    assert_eq!(
        scanned.len(),
        4,
        "each of the four rows appears exactly once"
    );
    assert_eq!(scanned, expected);
}

/// An empty store scans to an empty retention set.
#[tokio::test]
async fn file_retained_rows_on_empty_store_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_pinned_at(&dir, chrono::Utc::now());

    assert!(
        store.retained_rows().await.unwrap().is_empty(),
        "an empty store must retain nothing"
    );
}
