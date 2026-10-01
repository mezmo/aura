//! The retention-cleanup surface: checkpoint-presence classification, the
//! encapsulated evidence-first deletion order, and the sweep seam.
//!
//! Types and seams only — E6 (retention sweep and orphan classification)
//! and E8 (server bootstrap activation) own every body. Nothing here is
//! wired into the server yet: cleanup activates only after the lifetime,
//! reservation, consult, and retention bodies are filled and green.
#![allow(dead_code)] // the E6/E8 fills construct and drive this surface;
// the marker comes off when the sweep activates

use std::path::Path;

use super::lifetime::ReservationTable;
use super::resume::claim::{ResumeDocuments, ValidatedResumePath};
use super::resume::evaluate::Diagnostic;
use crate::hitl::PendingApprovals;

/// Whether a run's checkpoint is present, confirmed gone, or unreadable. A
/// missing root or an unreadable document is never confirmed absence: the
/// sweep retries, and unowned or corrupt files are reported as operational
/// diagnostics, never deleted by guessing.
#[derive(Debug, Clone)]
pub(crate) enum CheckpointPresence {
    /// A healthy checkpoint document exists under one of the two names:
    /// the run is not absent, and orphan collection does not apply.
    Present,
    /// No checkpoint under either name: confirmed gone.
    ConfirmedAbsent,
    /// The checkpoint root or document could not be read — absence is NOT
    /// confirmed and nothing may be deleted.
    Inaccessible(Diagnostic),
    /// A document exists under one of the names but does not decode: the
    /// run stays, reported as a diagnostic; corrupt evidence is never
    /// treated as absent evidence.
    Corrupt(Diagnostic),
}

/// One run's reservation-owning cleanup carrier: the lease that binds the
/// run and carries the cleanup eligibility (only an acquired reservation
/// may reread-and-delete), plus the run's two checkpoint paths derived
/// from the SAME validated identity the admission occupied. `Clone` hands
/// the SAME fence to the blocking reread/deletion tails, so the
/// reservation survives even if the awaiting sweep drops.
#[derive(Debug, Clone)]
pub(crate) struct CleanupReservation {
    reservation: super::lifetime::RunReservationLease,
    docs: ResumeDocuments,
}

/// Why a run is not cleanup-eligible: the admission itself is the
/// eligibility proof, so a live occupation (an executing run) is the one
/// refusal — retention revalidation under the acquired fence is E6's
/// body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CleanupAdmissionFault {
    /// A live reservation holds the run: execution is active and cleanup
    /// must not proceed. Nothing changed.
    Executing,
}

impl CleanupReservation {
    /// Acquire the run for cleanup: occupy the run under the shared
    /// reservation table — the admission IS the eligibility proof, never
    /// a clone of an executing run's lease — then derive the checkpoint
    /// paths from the same validated identity, so the fence and the
    /// paths cannot name different runs. A live run answers
    /// [`CleanupAdmissionFault::Executing`].
    pub(crate) fn acquire(
        table: &ReservationTable,
        path: &ValidatedResumePath,
        memory_dir: &str,
    ) -> Result<Self, CleanupAdmissionFault> {
        let reservation = table
            .admit(path.run.run_id())
            .map_err(|_| CleanupAdmissionFault::Executing)?;
        let docs = ResumeDocuments::for_path(path, memory_dir);
        Ok(Self { reservation, docs })
    }

    /// The reservation fencing this cleanup: the run's identity and its
    /// cleanup eligibility.
    pub(crate) fn reservation(&self) -> &super::lifetime::RunReservationLease {
        &self.reservation
    }

    /// The run's two checkpoint paths.
    pub(crate) fn documents(&self) -> &ResumeDocuments {
        &self.docs
    }
}

/// The outcome of one expired run's cleanup.
#[derive(Debug, Clone)]
pub(crate) enum RunCleanupOutcome {
    /// Owned approval and decision rows deleted first, then the checkpoint
    /// last: the run's evidence is gone.
    Removed,
    /// A deletion failed; the expired checkpoint is retained so the next
    /// startup or sweep can retry. Evidence that remains keeps answering
    /// `409 expired`; after full cleanup the run reads absent.
    RetainedForRetry(Diagnostic),
}

/// Inspect both checkpoint names for the run and classify presence — the
/// pre-deletion check the sweep runs under an acquired run reservation,
/// after re-reading the clock. The blocking reread tail holds the
/// carrier's lease reference through the work.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn inspect_checkpoint_presence(
    cleanup: &CleanupReservation,
) -> CheckpointPresence {
    todo!(
        "P45 wave fill unit E6: re-read both checkpoint names under the reservation and classify present vs confirmed absent vs inaccessible vs corrupt"
    )
}

/// Delete one expired run's evidence under the carrier's reservation,
/// strictly past `retention_expires_at` and with no execution active: owned
/// approval and decision rows first, the checkpoint LAST — so a failure
/// midway leaves the expired checkpoint to answer `409 expired` and the
/// next sweep can retry. The blocking deletion tail retains the carrier's
/// lease reference through the work.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn delete_expired_run(
    cleanup: &CleanupReservation,
    registry: &PendingApprovals,
) -> RunCleanupOutcome {
    todo!(
        "P45 wave fill unit E6: evidence-first, checkpoint-last deletion with retry-on-failure retention, fenced by the cleanup reservation"
    )
}

/// Scan one owned checkpoint root for retained run documents: the sweep's
/// per-root enumeration, off the async executor and without network work
/// under a lock. Each entry names a candidate run whose approval rows the
/// store's retained scan supplies; grouping and classification belong to
/// the sweep.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn scan_checkpoint_root(root: &Path) -> Result<Vec<String>, Diagnostic> {
    todo!(
        "P45 wave fill unit E6: enumerate one owned checkpoint root's run documents; a missing or unreadable root is a diagnostic, never confirmed absence"
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::str::FromStr;

    use super::{
        CheckpointPresence, CleanupAdmissionFault, CleanupReservation, RunCleanupOutcome,
        delete_expired_run, inspect_checkpoint_presence, scan_checkpoint_root,
    };
    use crate::hitl::{
        AcknowledgmentState, AgentScope, ApprovalAuthority, ApprovalItem, ApprovalOrigin,
        ApprovalRequest, DecisionId, PROTOCOL_VERSION, ParkedApproval, PendingApprovals,
    };
    use crate::orchestration::park::commit::{parked_document_dir, run_owner_id};
    use crate::orchestration::park::document::{
        PARKED_DOCUMENT_SUFFIX, ParkedPlan, ParkedRun, ParkedTaskNode, RESUMING_DOCUMENT_SUFFIX,
        SCHEMA_VERSION, load_parked_run,
    };
    use crate::orchestration::park::lifetime::ReservationTable;
    use crate::orchestration::park::resume::claim::{ResumeDocuments, ValidatedResumePath};
    use crate::orchestration::park::retention::RetentionExpiresAt;
    use crate::orchestration::{PendingCall, RunId, TaskIdentity, TaskStatus};

    /// Session segment every fixture path validates through.
    const SESSION: &str = "sess-cleanup";
    /// First fixture run id: a valid UUID v7 used for single-run tests.
    const RUN_A: &str = "0199c0de-4545-7000-8000-000000000045";
    /// Second fixture run id: a distinct valid UUID v7 used for enumeration.
    const RUN_B: &str = "0199c0de-4545-7000-8000-000000000046";
    /// Fixed decision id carried by the checkpoint's pending call.
    const DECISION: &str = "0199c0de-4545-7000-8000-000000000042";

    /// Parse a fixture run id through the crate's `RunId` `FromStr`.
    fn parse_run_id(raw: &str) -> RunId {
        RunId::from_str(raw).expect("fixture run id parses")
    }

    /// A validated resume path for the fixture session/run pair.
    fn path(session: &str, run: &str) -> ValidatedResumePath {
        ValidatedResumePath::parse(session, run).expect("fixture path validates")
    }

    /// The two checkpoint filenames for a fixture run.
    fn documents(memory_dir: &str, session: &str, run: &str) -> ResumeDocuments {
        ResumeDocuments::for_path(&path(session, run), memory_dir)
    }

    /// A fixed, strictly-past retention deadline so every fixture document
    /// reads as expired the moment the sweep inspects it.
    fn retention_expires_at() -> RetentionExpiresAt {
        RetentionExpiresAt::from_datetime(
            chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                .expect("fixture stamp parses")
                .with_timezone(&chrono::Utc),
        )
    }

    /// A realistic checkpoint document: one awaiting node with a gated pending
    /// call, mirroring the minimal field set the goldens module constructs.
    fn checkpoint_document(run_id: &str) -> ParkedRun {
        let decision_id = DecisionId::parse(DECISION).expect("fixture decision id parses");
        ParkedRun {
            schema_version: SCHEMA_VERSION,
            session_id: Some(SESSION.to_string()),
            run_id: run_id.to_string(),
            parked_at: "2026-09-01T00:00:00Z".to_string(),
            retention_expires_at: retention_expires_at(),
            query: "Deploy the service".to_string(),
            chat_history: vec![rig::completion::Message::user("Deploy the service")],
            coordinator_conversation: vec![],
            routing_decision: None,
            iteration: 1,
            planning_ms: 0,
            failure_history: vec![],
            plan: ParkedPlan {
                goal: "Deploy the service".to_string(),
                steps: None,
                tasks: vec![ParkedTaskNode {
                    task_id: 3,
                    description: "Gated apply".to_string(),
                    dependencies: vec![],
                    worker: Some("operations".to_string()),
                    rationale: String::new(),
                    status: TaskStatus::AwaitingApproval,
                    result: None,
                    error: None,
                    failure_category: None,
                    attempt: Some(1),
                    history: Some(vec![rig::completion::Message::user("apply it")]),
                    current_prompt: Some(rig::completion::Message::user("tool results")),
                    pending: Some(vec![PendingCall {
                        decision_id,
                        tool_name: "kubectl_apply".to_string(),
                        arguments: serde_json::json!({ "namespace": "prod" }),
                        call_id: "call_apply_1".to_string(),
                    }]),
                }],
            },
            executed: vec![],
            config_fingerprint: "fingerprint".to_string(),
            identity_hash: None,
            request_egress: HashMap::new(),
        }
    }

    /// Write raw bytes to `path` off the async executor, creating its parent
    /// directory first.
    async fn write_bytes(path: &Path, bytes: Vec<u8>) {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)
        })
        .await
        .expect("blocking write task joins")
        .expect("write bytes to path");
    }

    /// Write one checkpoint document to disk off the async executor.
    async fn write_document(path: &Path, document: &ParkedRun) {
        let bytes = serde_json::to_vec_pretty(document).expect("document serializes");
        write_bytes(path, bytes).await;
    }

    /// Stage a healthy checkpoint under the parked name.
    async fn stage_parked_document(memory_dir: &str, session: &str, run: &str) {
        let docs = documents(memory_dir, session, run);
        write_document(docs.parked(), &checkpoint_document(run)).await;
    }

    /// Stage a healthy checkpoint under the resuming name.
    async fn stage_resuming_document(memory_dir: &str, session: &str, run: &str) {
        let docs = documents(memory_dir, session, run);
        write_document(docs.resuming(), &checkpoint_document(run)).await;
    }

    /// Replace whatever is at `path` with a directory, so any file read or
    /// unlink targeting that path fails structurally. This is the portable
    /// failure fixture: it works on macOS even when the test runs as the
    /// file's owner.
    fn stage_directory_at(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(path);
        std::fs::create_dir(path).expect("stage directory at document path");
    }

    /// Construct one undecided approval owned by the run, the evidence the
    /// cleanup sweep must clear before (or alongside) deleting the checkpoint.
    fn run_approval(run_id: &str, decision_id: DecisionId) -> ParkedApproval {
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "cleanup-instance".to_string(),
                decision_id,
                request_id: run_owner_id(run_id),
                scope: AgentScope::Worker {
                    run_id: parse_run_id(run_id),
                    task: TaskIdentity::new(3, None),
                    session_id: None,
                },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "kubectl_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "kubectl_apply".to_string(),
                    tool_namespace: None,
                    arguments: serde_json::json!({ "namespace": "prod" }),
                    tool_call_intent: None,
                }],
            },
            registered_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(3600),
            authority: ApprovalAuthority::Conversational,
            egress_headers: None,
            acknowledgment: AcknowledgmentState::RequiresNotification,
        }
    }

    /// Stage one undecided approval in the registry under the run owner id.
    async fn stage_run_approval(registry: &PendingApprovals, run_id: &str) -> DecisionId {
        let decision_id = DecisionId::generate();
        registry
            .register_durable(run_approval(run_id, decision_id))
            .await
            .expect("stage run approval");
        decision_id
    }

    // Live-surface pin: `CleanupReservation::acquire` refuses a run that
    // holds a live execution reservation, mapping the live admission fault to
    // `CleanupAdmissionFault::Executing`. The reservation table and the
    // derived checkpoint paths are already green.

    #[tokio::test]
    async fn acquire_refuses_a_live_executing_run() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let table = ReservationTable::new();
        let run = parse_run_id(RUN_A);
        let _lease = table.admit(run).expect("admit the executing run");
        let result = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir);
        assert!(matches!(result, Err(CleanupAdmissionFault::Executing)));
    }

    // Hole: `inspect_checkpoint_presence` (P45 wave fill unit E6) — a
    // healthy checkpoint under the parked name classifies as Present; the run
    // is not absent and orphan collection does not apply.

    #[tokio::test]
    async fn inspect_classifies_parked_document_as_present() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        stage_parked_document(&memory_dir, SESSION, RUN_A).await;
        let docs = documents(&memory_dir, SESSION, RUN_A);
        let loaded = load_parked_run(docs.parked())
            .await
            .expect("fixture document loads and decodes");
        assert_eq!(loaded.run_id, RUN_A, "fixture pinned to the test run");

        let table = ReservationTable::new();
        let cleanup = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir)
            .expect("cleanup reservation admits");
        let presence = inspect_checkpoint_presence(&cleanup).await;
        assert!(matches!(presence, CheckpointPresence::Present));
    }

    // Hole: `inspect_checkpoint_presence` — a healthy checkpoint under the
    // resuming name alone also classifies as Present; either name satisfies.

    #[tokio::test]
    async fn inspect_classifies_resuming_document_as_present() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        stage_resuming_document(&memory_dir, SESSION, RUN_A).await;
        let docs = documents(&memory_dir, SESSION, RUN_A);
        let loaded = load_parked_run(docs.resuming())
            .await
            .expect("fixture document loads and decodes");
        assert_eq!(loaded.run_id, RUN_A, "fixture pinned to the test run");

        let table = ReservationTable::new();
        let cleanup = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir)
            .expect("cleanup reservation admits");
        let presence = inspect_checkpoint_presence(&cleanup).await;
        assert!(matches!(presence, CheckpointPresence::Present));
    }

    // Hole: `inspect_checkpoint_presence` — when neither the parked nor the
    // resuming filename exists, absence is confirmed. A missing document is
    // the only classification that may feed orphan deletion.

    #[tokio::test]
    async fn inspect_classifies_both_missing_as_confirmed_absent() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let docs = documents(&memory_dir, SESSION, RUN_A);
        assert!(!docs.parked().exists(), "fixture: parked file absent");
        assert!(!docs.resuming().exists(), "fixture: resuming file absent");

        let table = ReservationTable::new();
        let cleanup = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir)
            .expect("cleanup reservation admits");
        let presence = inspect_checkpoint_presence(&cleanup).await;
        assert!(matches!(presence, CheckpointPresence::ConfirmedAbsent));
    }

    // Hole: `inspect_checkpoint_presence` — a directory occupying one of the
    // document paths makes the checkpoint unreadable. The classification is
    // Inaccessible, never ConfirmedAbsent: unreadable evidence is not a
    // license to delete.

    #[tokio::test]
    async fn inspect_classifies_directory_at_path_as_inaccessible() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let docs = documents(&memory_dir, SESSION, RUN_A);
        stage_directory_at(docs.parked());
        assert!(
            docs.parked()
                .metadata()
                .map(|m| m.is_dir())
                .unwrap_or(false),
            "fixture: parked path must be a directory so read fails"
        );

        let table = ReservationTable::new();
        let cleanup = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir)
            .expect("cleanup reservation admits");
        let presence = inspect_checkpoint_presence(&cleanup).await;
        assert!(matches!(presence, CheckpointPresence::Inaccessible(_)));
    }

    // Hole: `inspect_checkpoint_presence` — bytes under one of the names that
    // do not decode as a `ParkedRun` classify as Corrupt. A corrupt file is
    // evidence of a problem, not evidence of absence.

    #[tokio::test]
    async fn inspect_classifies_invalid_bytes_as_corrupt() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let docs = documents(&memory_dir, SESSION, RUN_A);
        write_bytes(docs.parked(), b"not a parked run".to_vec()).await;
        let bytes = std::fs::read(docs.parked()).expect("fixture file reads");
        assert!(!bytes.is_empty(), "fixture: corrupt file holds bytes");

        let table = ReservationTable::new();
        let cleanup = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir)
            .expect("cleanup reservation admits");
        let presence = inspect_checkpoint_presence(&cleanup).await;
        assert!(matches!(presence, CheckpointPresence::Corrupt(_)));
    }

    // Hole: `delete_expired_run` (P45 wave fill unit E6) — idempotent
    // completion: with registry evidence present but the checkpoint already
    // gone, the outcome is Removed because nothing remains to retain.

    #[tokio::test]
    async fn delete_is_idempotent_when_checkpoint_is_already_gone() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        let registry = PendingApprovals::new();
        let decision_id = stage_run_approval(&registry, RUN_A).await;
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "fixture: approval evidence staged"
        );

        let table = ReservationTable::new();
        let cleanup = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir)
            .expect("cleanup reservation admits");
        let outcome = delete_expired_run(&cleanup, &registry).await;
        assert!(matches!(outcome, RunCleanupOutcome::Removed));
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_none(),
            "evidence is swept when there is no checkpoint to retain"
        );
    }

    // Hole: `delete_expired_run` — evidence-first, checkpoint-last order. When
    // the checkpoint unlink fails, the outcome is RetainedForRetry and the
    // registry evidence for the run is already gone. The retained checkpoint
    // can still answer `409 expired` on the next retry.

    #[tokio::test]
    async fn delete_sweeps_evidence_before_retaining_retry_on_unlink_failure() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        stage_parked_document(&memory_dir, SESSION, RUN_A).await;
        let docs = documents(&memory_dir, SESSION, RUN_A);
        assert!(docs.parked().is_file(), "fixture: parked document exists");

        let registry = PendingApprovals::new();
        let decision_id = stage_run_approval(&registry, RUN_A).await;
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_some(),
            "fixture: approval evidence staged"
        );

        // Replace the checkpoint file with a directory so the unlink fails
        // structurally; this avoids macOS owner-write permission subtleties.
        stage_directory_at(docs.parked());
        assert!(
            docs.parked()
                .metadata()
                .map(|m| m.is_dir())
                .unwrap_or(false),
            "fixture: parked path must be a directory so unlink fails"
        );

        let table = ReservationTable::new();
        let cleanup = CleanupReservation::acquire(&table, &path(SESSION, RUN_A), &memory_dir)
            .expect("cleanup reservation admits");
        let outcome = delete_expired_run(&cleanup, &registry).await;
        assert!(
            registry.try_parked(&decision_id).await.unwrap().is_none(),
            "registry evidence is swept before the checkpoint is retained"
        );
        assert!(matches!(outcome, RunCleanupOutcome::RetainedForRetry(_)));
    }

    // Hole: `scan_checkpoint_root` (P45 wave fill unit E6) — enumerate one
    // checkpoint root, stripping the parked and resuming suffixes to recover
    // run ids. A run with both names present contributes exactly one id.

    #[tokio::test]
    async fn scan_lists_one_run_per_document_pair() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let memory_dir = dir.path().to_string_lossy().to_string();
        stage_parked_document(&memory_dir, SESSION, RUN_A).await;
        stage_resuming_document(&memory_dir, SESSION, RUN_A).await;
        stage_parked_document(&memory_dir, SESSION, RUN_B).await;
        let root = parked_document_dir(&memory_dir, Some(SESSION));
        assert!(
            root.join(format!("{RUN_A}{PARKED_DOCUMENT_SUFFIX}"))
                .is_file()
        );
        assert!(
            root.join(format!("{RUN_A}{RESUMING_DOCUMENT_SUFFIX}"))
                .is_file()
        );
        assert!(
            root.join(format!("{RUN_B}{PARKED_DOCUMENT_SUFFIX}"))
                .is_file()
        );

        let mut ids = scan_checkpoint_root(&root).await.expect("scan succeeds");
        ids.sort();
        assert_eq!(ids, vec![RUN_A.to_string(), RUN_B.to_string()]);
    }

    // Hole: `scan_checkpoint_root` — a missing root is a diagnostic, never an
    // empty success. Confirmed absence of a root is not the same as confirmed
    // absence of every run under it.

    #[tokio::test]
    async fn scan_diagnoses_missing_root() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let missing = dir.path().join("no-such-root");
        assert!(!missing.exists(), "fixture: root is absent");
        let result = scan_checkpoint_root(&missing).await;
        assert!(result.is_err(), "missing root returns a diagnostic");
    }

    // Hole: `scan_checkpoint_root` — a file at the root path is unreadable as
    // a directory, so the scan returns a diagnostic rather than an empty list.

    #[tokio::test]
    async fn scan_diagnoses_unreadable_root() {
        let dir = tempfile::tempdir().expect("temp memory root");
        let root = dir.path().join("parked-root-file");
        std::fs::write(&root, b"not a directory").expect("stage file at root path");
        assert!(root.is_file(), "fixture: root is a file");
        let result = scan_checkpoint_root(&root).await;
        assert!(result.is_err(), "unreadable root returns a diagnostic");
    }
}
