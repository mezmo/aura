//! Conformance and contract tests for the file-backed approval store
//! (`AURA_SESSION_STORE=file`): the shared backend-agnostic battery plus the
//! §2.5 contract points specific to durable, retention-until-remove storage —
//! expiry refusal, retention past the decision, the approval's move into the
//! decision file, owner-scoped cancel of undecided approvals, and survival of a
//! process restart. The same battery runs against the memory backend to pin
//! the uniform contract. The skill-invocation store's own contract points —
//! restart survival, hashed session filenames, the per-session cap, skew
//! tolerance, and mtime-based expiry — follow. No Docker: every test gets its
//! own tempdir.

mod common;

use std::sync::Arc;
use std::time::Duration;

use aura::SessionId;
use aura::hitl::{ApprovalDecision, ResolveError};
use aura::session_store::{
    ApprovalStore, FileApprovalStore, FileSkillInvocationStore, InMemoryApprovalStore,
    MAX_SKILL_RECORDS_PER_SESSION, ParkedApprovalRecord, SKILL_INVOCATION_RECORD_VERSION,
    SessionStoreError, SkillInvocation, SkillInvocationRecord, SkillInvocationStore,
};
use aura_web_server::session_store::{FileSessionStore, SessionStore};

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
        store.resolve(&id, ApprovalDecision::Approved).await,
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
        .resolve(&id, ApprovalDecision::Approved)
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

/// §2.5: `resolve` moves the approval into the decision file rather than
/// deleting it — on disk the approval file is gone and the decision file
/// carries both the approval record and the decision.
#[tokio::test]
async fn resolve_moves_the_approval_into_the_decision_file() {
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
            .join("approvals")
            .join(format!("{id}.json"))
            .exists()
    );
    let decision_path = dir.path().join("decisions").join(format!("{id}.json"));
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(decision_path).unwrap()).unwrap();
    assert_eq!(on_disk["approval"]["decision_id"], id.to_string());
    assert_eq!(on_disk["approval"]["request_id"], "req-move");
    assert_eq!(on_disk["decision"]["approved"], true);
    assert_eq!(on_disk["decision"]["reason"], serde_json::Value::Null);
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
        .resolve(&decided_id, ApprovalDecision::Approved)
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
        Some(ApprovalDecision::Approved)
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
        .resolve(&decided_id, ApprovalDecision::Approved)
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
        Some(ApprovalDecision::Approved),
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
        .resolve(&id, ApprovalDecision::Approved)
        .await
        .expect("resolve commits without the approval removal");
    assert_eq!(
        store.decision(&id).await.unwrap(),
        Some(ApprovalDecision::Approved)
    );
    let restored = store
        .get(&id)
        .await
        .unwrap()
        .expect("approval record survives the failed removal");
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

// ---------------------------------------------------------------------------
// Skill-invocation store
// ---------------------------------------------------------------------------

fn skill_record(name: &str, anchor: u32, seq: u32) -> SkillInvocationRecord {
    SkillInvocationRecord {
        version: SKILL_INVOCATION_RECORD_VERSION,
        invocation: SkillInvocation::LoadSkill {
            name: name.to_string(),
        },
        tool_call_id: format!("call_{name}_{anchor}_{seq}"),
        anchor,
        seq,
        invoked_at: chrono::Utc::now(),
    }
}

fn skill_store(dir: &tempfile::TempDir, ttl_secs: Option<u64>) -> FileSkillInvocationStore {
    FileSkillInvocationStore::open(dir.path(), ttl_secs.and_then(std::num::NonZeroU64::new))
        .unwrap()
}

/// Every session log under the store, sorted. Filenames are hashes of the
/// session id, so tests locate a session's file by listing.
fn skill_files(dir: &tempfile::TempDir) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir.path().join("skills"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
}

/// Rewind a session file's mtime by `secs`.
fn age_file(path: &std::path::Path, secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
        .unwrap();
}

#[test]
fn skill_open_creates_the_skills_directory() {
    let dir = tempfile::tempdir().unwrap();
    let _store = skill_store(&dir, Some(60));
    assert!(dir.path().join("skills").is_dir());
}

#[tokio::test]
async fn skill_records_survive_a_reopen_in_anchor_order() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionId::new("sess-restart");
    {
        let store = skill_store(&dir, Some(60));
        store
            .record(&session, skill_record("beta", 1, 1))
            .await
            .unwrap();
        store
            .record(&session, skill_record("alpha", 1, 0))
            .await
            .unwrap();
    }

    let reopened = skill_store(&dir, Some(60));
    let labels: Vec<String> = reopened
        .list(&session)
        .await
        .unwrap()
        .iter()
        .map(|r| r.invocation.label())
        .collect();
    assert_eq!(labels, ["alpha", "beta"]);
}

#[tokio::test]
async fn skill_record_is_idempotent_and_first_write_wins() {
    let dir = tempfile::tempdir().unwrap();
    let store = skill_store(&dir, Some(60));
    let session = SessionId::new("sess-dup");
    store
        .record(&session, skill_record("alpha", 1, 0))
        .await
        .unwrap();
    store
        .record(&session, skill_record("alpha", 9, 4))
        .await
        .unwrap();

    let listed = store.list(&session).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].anchor, 1);
}

#[tokio::test]
async fn skill_sessions_are_isolated_and_a_hostile_id_stays_inside_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let store = skill_store(&dir, Some(60));
    let hostile = SessionId::new("../../etc/passwd");
    let other = SessionId::new("sess-other");
    store
        .record(&hostile, skill_record("alpha", 1, 0))
        .await
        .unwrap();

    assert!(store.list(&other).await.unwrap().is_empty());
    assert_eq!(store.list(&hostile).await.unwrap().len(), 1);
    let files = skill_files(&dir);
    assert_eq!(files.len(), 1);
    assert!(files[0].starts_with(dir.path().join("skills")));
}

#[tokio::test]
async fn skill_store_caps_records_per_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = skill_store(&dir, Some(60));
    let session = SessionId::new("sess-cap");
    for i in 0..(MAX_SKILL_RECORDS_PER_SESSION + 5) {
        store
            .record(&session, skill_record(&format!("s{i}"), i as u32, 0))
            .await
            .unwrap();
    }
    assert_eq!(
        store.list(&session).await.unwrap().len(),
        MAX_SKILL_RECORDS_PER_SESSION
    );

    // A re-invocation of a key the session already holds is never refused.
    store
        .record(&session, skill_record("s0", 99, 0))
        .await
        .unwrap();
    let listed = store.list(&session).await.unwrap();
    assert_eq!(listed.len(), MAX_SKILL_RECORDS_PER_SESSION);
    assert_eq!(listed[0].anchor, 0, "first write for a key still wins");
}

/// A corrupt line and a record on another schema version are skipped by
/// `list` and carried forward verbatim by the next write, so a newer
/// instance's records are never discarded by an older one.
#[tokio::test]
async fn skill_undecodable_lines_are_skipped_and_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let store = skill_store(&dir, Some(60));
    let session = SessionId::new("sess-skew");
    store
        .record(&session, skill_record("alpha", 1, 0))
        .await
        .unwrap();

    let path = skill_files(&dir).remove(0);
    let mut foreign = serde_json::to_value(skill_record("beta", 2, 0)).unwrap();
    foreign["version"] = serde_json::json!(SKILL_INVOCATION_RECORD_VERSION + 1);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("not json\n");
    text.push_str(&foreign.to_string());
    text.push('\n');
    std::fs::write(&path, text).unwrap();

    let listed = store.list(&session).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].invocation.label(), "alpha");

    store
        .record(&session, skill_record("gamma", 3, 0))
        .await
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("not json"));
    assert!(text.contains(&foreign.to_string()));
    assert_eq!(text.lines().count(), 4);
}

#[tokio::test]
async fn skill_expired_session_is_dropped_on_touch_and_a_write_refreshes_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = skill_store(&dir, Some(60));
    let session = SessionId::new("sess-ttl");
    store
        .record(&session, skill_record("alpha", 1, 0))
        .await
        .unwrap();
    let path = skill_files(&dir).remove(0);

    // A write inside the TTL rewrites the file, so an active session's older
    // records stay alive.
    age_file(&path, 30);
    store
        .record(&session, skill_record("beta", 2, 0))
        .await
        .unwrap();
    age_file(&path, 30);
    assert_eq!(store.list(&session).await.unwrap().len(), 2);

    age_file(&path, 61);
    assert!(store.list(&session).await.unwrap().is_empty());
    assert!(!path.exists(), "the expired file is removed on touch");
}

#[tokio::test]
async fn skill_open_sweeps_expired_session_files() {
    let dir = tempfile::tempdir().unwrap();
    let live = SessionId::new("sess-live");
    let stale = SessionId::new("sess-stale");
    {
        let store = skill_store(&dir, Some(60));
        store
            .record(&stale, skill_record("alpha", 1, 0))
            .await
            .unwrap();
        age_file(&skill_files(&dir).remove(0), 120);
        store
            .record(&live, skill_record("alpha", 1, 0))
            .await
            .unwrap();
    }
    assert_eq!(skill_files(&dir).len(), 2);

    let reopened = skill_store(&dir, Some(60));
    assert_eq!(skill_files(&dir).len(), 1, "open removes the expired file");
    assert_eq!(reopened.list(&live).await.unwrap().len(), 1);
    assert!(reopened.list(&stale).await.unwrap().is_empty());
}

#[tokio::test]
async fn skill_store_without_ttl_never_expires() {
    let dir = tempfile::tempdir().unwrap();
    let store = skill_store(&dir, None);
    let session = SessionId::new("sess-forever");
    store
        .record(&session, skill_record("alpha", 1, 0))
        .await
        .unwrap();
    age_file(&skill_files(&dir).remove(0), 10 * 365 * 24 * 3600);
    assert_eq!(store.list(&session).await.unwrap().len(), 1);
}

/// The file session store hands out the file-backed skill store, so a skill
/// log written before a restart is readable after it and the health probe
/// covers its directory.
#[tokio::test]
async fn file_session_store_skill_log_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = aura_config::FileSessionStoreConfig {
        path: dir.path().to_string_lossy().into_owned(),
        skills_ttl_secs: std::num::NonZeroU64::new(60),
    };
    let session = SessionId::new("sess-wired");
    FileSessionStore::new(&config)
        .unwrap()
        .skills()
        .record(&session, skill_record("alpha", 1, 0))
        .await
        .unwrap();

    let reopened = FileSessionStore::new(&config).unwrap();
    assert_eq!(reopened.skills().list(&session).await.unwrap().len(), 1);
    reopened.ping().await.unwrap();
}
