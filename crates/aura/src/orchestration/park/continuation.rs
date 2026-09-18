//! The continuation surfaces: the per-run resuming-document handle and the
//! typed rehydrate errors.
//!
//! Distinct-owner note (P44 frontier finding): [`ResumingDocumentHandle`] is
//! the park-module's per-run handle for appending tombstones to a resuming
//! document. It is **not** the endpoint-owned claim table (P45), which tracks
//! which endpoint holds a run's resume; the two surfaces stay separate on
//! purpose.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::hitl::{
    AddressedApproval, ApprovalAuthority, ApprovalRead, DecisionId, PendingApprovals,
};
use crate::orchestration::park::document::{ParkedRun, RESUMING_DOCUMENT_SUFFIX, load_parked_run};
use crate::orchestration::park::recorded_decisions::{CallKey, RecordedDecisions};
use crate::orchestration::persistence::is_safe_path_component;

use super::lifetime::RunExecutionScope;
use super::resume::{BlockingEntry, ParkedToolName};

/// Why a run could not rehydrate, mapped to the section 2.6 condition rows.
#[derive(Debug)]
pub(crate) enum RehydrateError {
    /// Condition row "not found": no checkpoint document exists for the run.
    NotFound,
    /// Condition row "expired": the run is past `retention_expires_at`.
    /// Carries the pending snapshots the consult collected — each
    /// outstanding call with its own per-call deadline — possibly empty
    /// when every member was addressed before the retention deadline (the
    /// terminal row is honest either way). Retention expiry is terminal
    /// for the checkpoint: past the stamp the run tears down whatever the
    /// members' states, and only an inside-retention run resumes.
    Expired { blocking: Vec<BlockingEntry> },
    /// Condition row "mismatch": the store and the document disagree — the
    /// stored approval is missing, its scope names another run or task than
    /// the checkpoint node, or its recorded call differs from the
    /// document's.
    Mismatch(String),
    /// Condition row "parked": a pending call still has no recorded
    /// decision inside the decision window. Carries the pending snapshots —
    /// each outstanding call with its own per-call deadline — the resume
    /// endpoint renders as the 409 `parked` body; an all-decided resume
    /// never sees this.
    Parked { blocking: Vec<BlockingEntry> },
    /// Condition row "config_changed": the fingerprint no longer matches the
    /// rebuilt configuration. Checked by the resume endpoint against a
    /// header-resolved config entry point (P45 adoption).
    #[allow(dead_code)]
    // reserved: the fingerprint row is enforced structurally ahead of the consult
    ConfigChanged,
    /// Condition row carried as a store fault: the approval store failed
    /// mid-read.
    Store(String),
    /// Condition row carried as a document fault: the document could not be
    /// read or decoded.
    Document(String),
}

impl std::fmt::Display for RehydrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no checkpoint document for the run"),
            Self::Expired { .. } => write!(f, "the run's decision window has expired"),
            Self::Mismatch(detail) => write!(f, "resume mismatch: {detail}"),
            Self::Parked { blocking } => write!(
                f,
                "{} approval(s) still await a decision inside the window",
                blocking.len()
            ),
            Self::ConfigChanged => write!(f, "configuration changed since the run parked"),
            Self::Store(detail) => write!(f, "approval store read failed: {detail}"),
            Self::Document(detail) => write!(f, "checkpoint document read failed: {detail}"),
        }
    }
}

/// The per-run mutex over a resuming document plus its append-and-publish
/// API. `open` loads the published parked document; every
/// [`Self::append_executed_and_publish`] serializes the mutated document to a
/// temp file and renames it over `{run_id}.resuming.json`, holding the
/// per-run lock across clone-mutate-write so concurrent sibling continuations
/// in one wave cannot lose each other's entries.
///
/// This is the park-module handle, distinct from P45's endpoint-owned claim
/// table (see the module docs).
#[derive(Debug)]
pub(crate) struct ResumingDocumentHandle {
    document: tokio::sync::Mutex<ParkedRun>,
    /// The file appends publish to: `{parked_dir}/{run_id}.resuming.json`.
    publish_path: PathBuf,
    /// The run's execution scope, when the handle is built on a resume-bound
    /// run: every append's blocking write-rename tail is spawned TRACKED
    /// through it, holding a lease reference until the rename completes.
    /// `None` for an unscoped construction (bare spawn, byte-equivalent).
    execution_scope: Option<Arc<RunExecutionScope>>,
}

impl ResumingDocumentHandle {
    /// Load the parked document at `path` and arm the handle. Appends publish
    /// to the sibling `{run_id}.resuming.json`; the parked document itself is
    /// left untouched.
    pub(crate) async fn open(
        path: &Path,
        execution_scope: Option<Arc<RunExecutionScope>>,
    ) -> Result<Self, RehydrateError> {
        let document = match load_parked_run(path).await {
            Ok(document) => document,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(RehydrateError::NotFound);
            }
            Err(e) => return Err(RehydrateError::Document(e.to_string())),
        };
        let run_id = document.run_id.clone();
        if !is_safe_path_component(&run_id) {
            return Err(RehydrateError::Document(format!(
                "invalid run id for parked document: {run_id:?}"
            )));
        }
        let parked_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let publish_path = parked_dir.join(format!("{run_id}{RESUMING_DOCUMENT_SUFFIX}"));
        Ok(Self {
            document: tokio::sync::Mutex::new(document),
            publish_path,
            execution_scope,
        })
    }

    /// Append one executed call id and publish the document: lock, push,
    /// temp write, same-directory rename. The lock is held across the write,
    /// so the published document is always the full executed set and no
    /// sibling append can interleave.
    pub(crate) async fn append_executed_and_publish(&self, call_id: &str) -> std::io::Result<()> {
        let mut document = self.document.lock().await;
        document.executed.push(call_id.to_string());
        let bytes = serde_json::to_vec_pretty(&*document)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let publish_path = self.publish_path.clone();
        let tmp = publish_path.with_file_name(format!(
            ".{}.tmp",
            publish_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        let write = move || -> std::io::Result<()> {
            if let Some(parent) = non_empty_parent(&publish_path) {
                crate::session_store::private_dir(parent)?;
            }
            crate::session_store::write_private(&tmp, &bytes)?;
            std::fs::rename(&tmp, &publish_path)
        };
        match self.execution_scope.as_ref() {
            Some(scope) => scope.spawn_blocking_tracked(write).await,
            None => tokio::task::spawn_blocking(write).await,
        }
        .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// The executed tombstones recorded so far.
    #[cfg(test)]
    pub(crate) async fn executed(&self) -> Vec<String> {
        self.document.lock().await.executed.clone()
    }

    /// The publish path, for tests asserting the resuming document on disk.
    #[cfg(test)]
    pub(crate) fn publish_path(&self) -> &Path {
        &self.publish_path
    }
}

/// Read the run's recorded decisions out of the store, keyed the way the
/// resume gate consumes them. Every awaiting node's pending call is read
/// through the store's ONE authority-aware read-or-expire boundary
/// ([`ApprovalAuthority::WebhookPoll`], the authority production park rows
/// register under): the store owns authority, deadline, and terminal-winner
/// arbitration under its serialization boundary, so a row whose own deadline
/// has passed comes back `Addressed TimedOut` carrying its durable deadline,
/// and an already-decided row comes back `Addressed Decided` — never
/// re-derived from the document, and never a fabricated decision. The
/// document's recorded call must still match the stored approval, or the
/// resume is a mismatch. The approval must be a park-retained one: park mode
/// requires the file-backed store, whose `get` returns the approval before
/// and after the decision.
///
/// The caller injects `now`, which now drives ONLY the run-wide retention
/// check: per-call deadline arbitration belongs to the store's clock inside
/// `read_or_expire`, and the pending snapshots carry each call's own
/// deadline. The loop reads EVERY member exactly once; a store fault aborts
/// immediately, while a present-row identity violation and a missing row are
/// collected so the loop can finish. The after-loop precedence is:
/// present-row mismatch outranks the run-wide expiry (a stored row that
/// cannot decide this call is the 2.6 mismatch whatever the clock), which
/// outranks a missing-inside-the-window mismatch, which outranks the parked
/// row; otherwise the run resumes ready.
pub(crate) async fn load_recorded_decisions(
    store: &PendingApprovals,
    doc: &ParkedRun,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Arc<RecordedDecisions>, Vec<DecisionId>), RehydrateError> {
    let recorded = Arc::new(RecordedDecisions::default());
    let mut decision_ids = Vec::new();
    let mut blocking = Vec::new();
    // The FIRST present row whose identity violates validation: it outranks
    // the expired and parked outcomes, so it is collected and returned only
    // after every member has been read (the one-read-per-member contract).
    let mut present_mismatch: Option<String> = None;
    // The FIRST missing row: inside the window it is the mismatch row, but
    // only after the present-row mismatch and the run-wide window checks.
    let mut missing: Option<DecisionId> = None;
    // The document's retention stamp is the run-wide decision window the 2.6
    // expired row is evaluated against; each call's own deadline lives in
    // the store and is reported through its pending snapshot.
    let expires_at = doc.retention_expires_at.as_datetime();

    for node in &doc.plan.tasks {
        let crate::orchestration::types::TaskStatus::AwaitingApproval = node.status else {
            continue;
        };
        let Some(pending) = &node.pending else {
            continue;
        };
        for call in pending {
            let read = store
                .read_or_expire(&call.decision_id, ApprovalAuthority::WebhookPoll)
                .await
                .map_err(|e| RehydrateError::Store(e.to_string()))?;
            let (parked, outcome) = match read {
                ApprovalRead::Missing => {
                    // A missing row has no per-call deadline and no identity
                    // to validate; remember the FIRST so the after-loop
                    // precedence can answer it inside the window, and keep
                    // reading the remaining members — a later PRESENT row
                    // that mismatches outranks the run-wide expiry.
                    if missing.is_none() {
                        missing = Some(call.decision_id);
                    }
                    continue;
                }
                ApprovalRead::Pending(parked) => (parked, None),
                ApprovalRead::Addressed { approval, outcome } => (approval, Some(outcome)),
            };
            // The stored approval must name this run and this checkpoint
            // node, carry one item, and match the documented call. A present
            // row that violates any of these outranks the expired and parked
            // outcomes alike: collect the FIRST such violation and finish the
            // loop, so every member is still read exactly once.
            let identity: Result<&crate::hitl::ApprovalItem, String> = 'identity: {
                match &parked.request.scope {
                    crate::hitl::AgentScope::Worker { run_id, task, .. } => {
                        if run_id.to_string() != doc.run_id {
                            break 'identity Err(format!(
                                "approval {} belongs to run {run_id}, not this run",
                                call.decision_id
                            ));
                        }
                        if task.task_id != node.task_id {
                            break 'identity Err(format!(
                                "approval {} belongs to task {}, not task {}",
                                call.decision_id, task.task_id, node.task_id
                            ));
                        }
                    }
                    crate::hitl::AgentScope::Single { .. } => {
                        break 'identity Err(format!(
                            "approval {} carries a single-agent scope",
                            call.decision_id
                        ));
                    }
                    crate::hitl::AgentScope::Coordinator { .. } => {
                        break 'identity Err(format!(
                            "approval {} carries a coordinator scope",
                            call.decision_id
                        ));
                    }
                }
                // The approval is single-item by construction (one parked
                // call raises one request); items[0] is the call the human
                // decided on.
                let Some(item) = parked.request.items.first() else {
                    break 'identity Err(format!(
                        "approval {} carries no approval item",
                        call.decision_id
                    ));
                };
                if item.tool_name != call.tool_name || item.arguments != call.arguments {
                    break 'identity Err(format!(
                        "documented call {}({}) does not match the stored approval",
                        call.tool_name, call.decision_id
                    ));
                }
                Ok(item)
            };
            let item = match identity {
                Ok(item) => item,
                Err(detail) => {
                    if present_mismatch.is_none() {
                        present_mismatch = Some(detail);
                    }
                    continue;
                }
            };
            // The key's task id comes from the awaiting node, the tool name
            // and arguments from the store's approval record. The carrier
            // keeps the recorded identity with the decision it rode in with.
            match outcome {
                // Still parked inside its own window: the snapshot the 409
                // body renders, carrying THIS call's own deadline.
                None => blocking.push(BlockingEntry {
                    decision_id: call.decision_id,
                    tool: ParkedToolName::new(call.tool_name.clone()),
                    expires_at: parked.expires_at,
                }),
                Some(AddressedApproval::Decided(resolved)) => {
                    recorded.push(
                        CallKey::new(node.task_id, &item.tool_name, &item.arguments),
                        AddressedApproval::Decided(resolved),
                    );
                    // A decided call is consumed from the store; the id the
                    // re-park cleanup releases.
                    decision_ids.push(call.decision_id);
                }
                Some(AddressedApproval::TimedOut { deadline }) => {
                    // The store already published the durable terminal record
                    // and removed the undecided half: nothing pending remains
                    // to consume, so the id joins neither the consumed list
                    // nor the pending snapshots.
                    recorded.push(
                        CallKey::new(node.task_id, &item.tool_name, &item.arguments),
                        AddressedApproval::TimedOut { deadline },
                    );
                }
            }
        }
    }
    // After every member has been read: a present-row identity mismatch
    // outranks the run-wide window (a stored row that cannot decide this call
    // is the 2.6 mismatch whatever the clock). Past retention the terminal
    // expired row follows, carrying the pending snapshots collected so far
    // (possibly empty when every member was addressed before the window
    // closed). Inside the window a missing row is the mismatch, the pending
    // snapshots are the parked row, and otherwise the run resumes ready.
    if let Some(detail) = present_mismatch {
        return Err(RehydrateError::Mismatch(detail));
    }
    if now > expires_at {
        return Err(RehydrateError::Expired { blocking });
    }
    if let Some(decision_id) = missing {
        return Err(RehydrateError::Mismatch(format!(
            "store approval {decision_id} is missing"
        )));
    }
    if !blocking.is_empty() {
        return Err(RehydrateError::Parked { blocking });
    }

    Ok((recorded, decision_ids))
}

/// The directory a document lives in, or `None` for a bare file name whose
/// parent is the empty path.
fn non_empty_parent(path: &std::path::Path) -> Option<&std::path::Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn non_empty_parent_skips_a_bare_file_name() {
        use std::path::Path;
        assert_eq!(super::non_empty_parent(Path::new("run.json")), None);
        assert_eq!(
            super::non_empty_parent(Path::new("parked/run.json")),
            Some(Path::new("parked"))
        );
    }

    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalAuthority, ApprovalDecision, ApprovalItem, ApprovalOrigin,
        ApprovalRequest, PROTOCOL_VERSION, ParkedApproval, ResolvedDecision,
    };
    use crate::orchestration::park::document::{ParkedPlan, ParkedTaskNode, SCHEMA_VERSION};
    use crate::orchestration::park::retention::RetentionExpiresAt;
    use crate::orchestration::types::PendingCall;
    use crate::orchestration::types::TaskStatus;

    fn parked_run(pending: Vec<PendingCall>) -> ParkedRun {
        ParkedRun {
            schema_version: SCHEMA_VERSION,
            session_id: Some("sess".to_string()),
            run_id: "0191e8c0-aaaa-7000-8000-00000000c0de".to_string(),
            parked_at: "2026-09-02T14:00:00+00:00".to_string(),
            retention_expires_at: RetentionExpiresAt::from_datetime(
                chrono::Utc::now() + chrono::Duration::hours(1),
            ),
            query: "Deploy".to_string(),
            chat_history: vec![],
            coordinator_conversation: vec![],
            routing_decision: None,
            iteration: 1,
            planning_ms: 0,
            failure_history: vec![],
            plan: ParkedPlan {
                goal: "Deploy".to_string(),
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
                    history: None,
                    current_prompt: None,
                    pending: Some(pending),
                }],
            },
            executed: vec![],
            config_fingerprint: "f".to_string(),
            identity_hash: None,
        }
    }

    fn pending_call(decision_id: DecisionId, args: serde_json::Value) -> PendingCall {
        PendingCall {
            decision_id,
            tool_name: "kubectl_apply".to_string(),
            arguments: args,
            call_id: "call_1".to_string(),
        }
    }

    fn approval(decision_id: DecisionId, args: serde_json::Value) -> ParkedApproval {
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "test-instance".to_string(),
                decision_id,
                request_id: "run:test".to_string(),
                scope: AgentScope::Worker {
                    run_id: "0191e8c0-aaaa-7000-8000-00000000c0de".parse().unwrap(),
                    task: crate::orchestration::types::TaskIdentity::new(3, None),
                    session_id: None,
                },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "kubectl_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "kubectl_apply".to_string(),
                    arguments: args,
                    tool_call_intent: None,
                }],
            },
            registered_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            // The consult presents the authority production park rows
            // register under; the fixture matches the new read seam.
            authority: ApprovalAuthority::WebhookPoll,
            egress_headers: None,
            acknowledgment: crate::hitl::AcknowledgmentState::RequiresNotification,
        }
    }

    fn file_store() -> (PendingApprovals, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::session_store::FileApprovalStore::open(dir.path()).unwrap();
        (
            PendingApprovals::with_backend(
                Arc::new(store),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            ),
            dir,
        )
    }

    /// Entries are built from the store's approval record: a hit with the
    /// store's tool name/arguments consumes at the gate, the decision
    /// survives the resolve (the file backend moves the approval into the
    /// decision record), and the returned ids carry every consumed id.
    #[tokio::test]
    async fn load_builds_entries_from_the_store_not_the_document() {
        let (registry, _dir) = file_store();
        let decision_id = DecisionId::generate();
        let args = serde_json::json!({ "namespace": "prod" });
        registry
            .register_durable(approval(decision_id, args.clone()))
            .await
            .unwrap();
        registry
            .resolve(
                &decision_id,
                ApprovalAuthority::WebhookPoll,
                ApprovalDecision::Approved.into(),
            )
            .await
            .unwrap();

        // The document's copy disagrees on the arguments: the mismatch row
        // refuses the divergence before any entry is built.
        let mismatched = parked_run(vec![pending_call(
            decision_id,
            serde_json::json!({ "namespace": "stage" }),
        )]);
        let err = load_recorded_decisions(&registry, &mismatched, chrono::Utc::now())
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match the stored approval"),
            "got: {err}"
        );

        // The matching shape: the entry consumes at the resume gate.
        let doc = parked_run(vec![pending_call(decision_id, args.clone())]);
        let (recorded, ids) = load_recorded_decisions(&registry, &doc, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(ids, vec![decision_id]);
        assert_eq!(
            recorded.take(&CallKey::new(3, "kubectl_apply", &args)),
            Some(AddressedApproval::Decided(ResolvedDecision::from(
                ApprovalDecision::Approved
            ))),
            "the recorded decision is consumable at the resume gate"
        );
        assert!(
            recorded
                .take(&CallKey::new(3, "kubectl_apply", &args))
                .is_none()
        );
    }

    /// A pending call whose approval is gone from the store is a mismatch
    /// (the 2.6 mismatch row names a missing approval); one still parked but
    /// undecided is the parked row inside the window and the expired row
    /// past it.
    #[tokio::test]
    async fn missing_approval_is_mismatch_and_undecided_follows_the_window() {
        let (registry, _dir) = file_store();
        let vanished = DecisionId::generate();
        let undecided = DecisionId::generate();
        let args = serde_json::json!({});
        registry
            .register_durable(approval(undecided, args.clone()))
            .await
            .unwrap();

        let err = load_recorded_decisions(
            &registry,
            &parked_run(vec![pending_call(vanished, args.clone())]),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("store approval") && err.to_string().contains("missing"),
            "got: {err}"
        );

        let err = load_recorded_decisions(
            &registry,
            &parked_run(vec![pending_call(undecided, args.clone())]),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
        match err {
            RehydrateError::Parked { blocking } => {
                assert_eq!(blocking.len(), 1, "one outstanding member: {blocking:?}");
                assert_eq!(
                    blocking[0].decision_id, undecided,
                    "the parked snapshot names the undecided call"
                );
            }
            other => panic!("expected Parked with the outstanding id, got: {other}"),
        }

        // The same undecided call past the document's expiry is the expired
        // row.
        let mut doc = parked_run(vec![pending_call(undecided, args.clone())]);
        doc.retention_expires_at =
            RetentionExpiresAt::from_datetime(chrono::Utc::now() - chrono::Duration::seconds(1));
        let err = load_recorded_decisions(&registry, &doc, chrono::Utc::now())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("expired"), "got: {err}");

        // A row the store already swept past the window is expired, not a
        // mismatch.
        doc.plan.tasks[0].pending = Some(vec![pending_call(vanished, args)]);
        let err = load_recorded_decisions(&registry, &doc, chrono::Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, RehydrateError::Expired { .. }), "got: {err}");
    }

    /// The stored approval's scope must name this run and this checkpoint
    /// node: wrong run, wrong task, and non-worker scopes are each the
    /// mismatch row (the other-run/other-task borrow of T1).
    #[tokio::test]
    async fn scope_mismatch_refuses_the_borrowed_approval() {
        let (registry, _dir) = file_store();
        let decision_id = DecisionId::generate();
        let args = serde_json::json!({ "namespace": "prod" });

        let mut other_run = approval(decision_id, args.clone());
        other_run.request.scope = crate::hitl::AgentScope::Worker {
            run_id: "0191e8c0-bbbb-7000-8000-00000000c0de".parse().unwrap(),
            task: crate::orchestration::types::TaskIdentity::new(3, None),
            session_id: None,
        };
        registry.register_durable(other_run).await.unwrap();
        let err = load_recorded_decisions(
            &registry,
            &parked_run(vec![pending_call(decision_id, args.clone())]),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not this run"), "got: {err}");

        let (registry, _dir) = file_store();
        let mut other_task = approval(decision_id, args.clone());
        other_task.request.scope = crate::hitl::AgentScope::Worker {
            run_id: "0191e8c0-aaaa-7000-8000-00000000c0de".parse().unwrap(),
            task: crate::orchestration::types::TaskIdentity::new(7, None),
            session_id: None,
        };
        registry.register_durable(other_task).await.unwrap();
        let err = load_recorded_decisions(
            &registry,
            &parked_run(vec![pending_call(decision_id, args.clone())]),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not task 3"), "got: {err}");

        let (registry, _dir) = file_store();
        let mut coordinator_scope = approval(decision_id, args);
        coordinator_scope.request.scope = crate::hitl::AgentScope::Coordinator {
            run_id: "0191e8c0-aaaa-7000-8000-00000000c0de".parse().unwrap(),
        };
        registry.register_durable(coordinator_scope).await.unwrap();
        let err = load_recorded_decisions(
            &registry,
            &parked_run(vec![pending_call(decision_id, serde_json::json!({}))]),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("coordinator scope"), "got: {err}");
    }

    /// Decision-during-refresh correction: the document's pending set still
    /// says undecided, but the store holds the decision —
    /// `load_recorded_decisions` returns it (the store corrects the document
    /// at resume, the P43 lenient-refresh ruling's backing condition).
    #[tokio::test]
    async fn store_decision_corrects_a_document_left_pending() {
        let (registry, _dir) = file_store();
        let decision_id = DecisionId::generate();
        let args = serde_json::json!({ "namespace": "prod" });
        registry
            .register_durable(approval(decision_id, args.clone()))
            .await
            .unwrap();
        // The decision lands after the document was committed.
        registry
            .resolve(
                &decision_id,
                ApprovalAuthority::WebhookPoll,
                ApprovalDecision::Approved.into(),
            )
            .await
            .unwrap();

        let doc = parked_run(vec![pending_call(decision_id, args.clone())]);
        let (recorded, ids) = load_recorded_decisions(&registry, &doc, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(ids, vec![decision_id]);
        assert_eq!(
            recorded.take(&CallKey::new(3, "kubectl_apply", &args)),
            Some(AddressedApproval::Decided(ResolvedDecision::from(
                ApprovalDecision::Approved
            )))
        );
    }

    /// Two concurrent appends serialize through the per-run lock: both
    /// entries land, the published document carries the full executed set,
    /// and no temp residue is left.
    #[tokio::test]
    async fn concurrent_appends_serialize_through_the_per_run_lock() {
        let dir = tempfile::tempdir().unwrap();
        let parked_path = dir.path().join("0191e8c0-aaaa-7000-8000-00000000c0de.json");
        std::fs::write(
            &parked_path,
            serde_json::to_vec(&parked_run(vec![])).unwrap(),
        )
        .unwrap();

        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let handle = Arc::new(
            ResumingDocumentHandle::open(&parked_path, None)
                .await
                .unwrap(),
        );
        let h1 = Arc::clone(&handle);
        let h2 = Arc::clone(&handle);
        let (a, b) = tokio::join!(
            h1.append_executed_and_publish("call_a"),
            h2.append_executed_and_publish("call_b")
        );
        a.unwrap();
        b.unwrap();

        let mut executed = handle.executed().await;
        executed.sort();
        assert_eq!(executed, vec!["call_a".to_string(), "call_b".to_string()]);

        let published: ParkedRun =
            serde_json::from_str(&std::fs::read_to_string(handle.publish_path()).unwrap()).unwrap();
        assert_eq!(published.executed.len(), 2, "no lost sibling entries");
        assert_eq!(
            handle.publish_path().file_name().unwrap().to_string_lossy(),
            format!("0191e8c0-aaaa-7000-8000-00000000c0de{RESUMING_DOCUMENT_SUFFIX}")
        );
        assert!(
            parked_path.try_exists().unwrap(),
            "the parked document is untouched"
        );
        let residue = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!residue, "no temp file residue");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(handle.publish_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the resuming document is owner-only");
            let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                dir_mode, 0o700,
                "the parked directory is tightened on append"
            );
        }
    }

    /// A missing document opens as NotFound — the section 2.6 "not found"
    /// row.
    #[tokio::test]
    async fn open_reports_not_found_for_a_missing_document() {
        let dir = tempfile::tempdir().unwrap();
        let err = ResumingDocumentHandle::open(&dir.path().join("absent.json"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, RehydrateError::NotFound));
    }
}
