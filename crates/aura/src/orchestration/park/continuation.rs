//! The continuation surfaces: the checkpointed worker conversation a resume
//! drives, the per-run resuming-document handle, and the typed rehydrate
//! errors.
//!
//! Distinct-owner note (P44 frontier finding): [`ResumingDocumentHandle`] is
//! the park-module's per-run handle for appending tombstones to a resuming
//! document. It is **not** the endpoint-owned claim table (P45), which tracks
//! which endpoint holds a run's resume; the two surfaces stay separate on
//! purpose.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rig::completion::Message;

use crate::hitl::{DecisionId, PendingApprovals};
use crate::orchestration::park::document::{ParkedRun, RESUMING_DOCUMENT_SUFFIX, load_parked_run};
use crate::orchestration::park::recorded_decisions::{CallKey, RecordedDecisions};
use crate::orchestration::persistence::is_safe_path_component;
use crate::orchestration::types::PendingCall;

/// One parked task's continuation payload: the recorded attempt, the
/// conversation captured when the worker parked, and the calls awaiting a
/// decision in recorded order. Built by the resume path from a checkpoint
/// document (and so carries the restored coordinator-facing state's
/// worker-side half).
#[derive(Debug, Clone)]
pub(crate) struct TaskContinuation {
    /// The worker attempt that blocked; the resume rebuilds the worker for
    /// the same attempt number.
    pub attempt: usize,
    /// Everything before the final worker turn.
    pub history: Vec<Message>,
    /// The final worker turn's aggregated tool-result prompt, still carrying
    /// one sentinel per parked call id.
    pub current_prompt: Message,
    /// The parked calls, in the order the continuation invokes them.
    pub pending: Vec<PendingCall>,
}

/// Everything the continuation arm needs besides the checkpoint itself: the
/// run's recorded decisions (behind the `Arc` the gate already holds) and the
/// resuming document every tombstone appends through.
#[derive(Clone)]
pub(crate) struct ResumeContext {
    pub recorded: Arc<RecordedDecisions>,
    pub document: Arc<ResumingDocumentHandle>,
}

/// Why a run could not rehydrate, mapped to the section 2.6 condition rows.
#[allow(dead_code)] // P45 resume endpoint consumes the rehydrate entry points
#[derive(Debug)]
pub(crate) enum RehydrateError {
    /// Condition row "not found": no checkpoint document exists for the run.
    NotFound,
    /// Condition row "expired": a pending call has no recorded decision and
    /// the run is past `expires_at`. A run whose decisions were all recorded
    /// in time resumes after expiry — the window bounds the decision, not
    /// the resumer.
    Expired,
    /// Condition row "mismatch": the store and the document disagree — the
    /// stored approval is missing, its scope names another run or task than
    /// the checkpoint node, or its recorded call differs from the
    /// document's.
    Mismatch(String),
    /// Condition row "parked": a pending call still has no recorded
    /// decision inside the decision window. The resume endpoint answers
    /// 409 `parked` with the outstanding ids and `expires_at`; an
    /// all-decided resume never sees this.
    Parked {
        outstanding: Vec<DecisionId>,
        expires_at: chrono::DateTime<chrono::Utc>,
    },
    /// Condition row "config_changed": the fingerprint no longer matches the
    /// rebuilt configuration. Checked by the resume endpoint against a
    /// header-resolved config entry point (P45 adoption).
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
            Self::Expired => write!(f, "the run's decision window has expired"),
            Self::Mismatch(detail) => write!(f, "resume mismatch: {detail}"),
            Self::Parked { outstanding, .. } => write!(
                f,
                "{} approval(s) still await a decision inside the window",
                outstanding.len()
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
}

impl ResumingDocumentHandle {
    /// Load the parked document at `path` and arm the handle. Appends publish
    /// to the sibling `{run_id}.resuming.json`; the parked document itself is
    /// left untouched.
    #[allow(dead_code)] // P45 resume endpoint consumes the rehydrate entry points
    pub(crate) async fn open(path: &Path) -> Result<Self, RehydrateError> {
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
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &publish_path)
        })
        .await
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
/// resume gate consumes them. For each awaiting node's pending call the
/// stored approval (`try_parked`, the `items[0]` the human saw) and the
/// recorded decision (`recorded_decision`) come from the **store**, never the
/// document's copy — the store corrects the document at resume (the P43
/// lenient-refresh ruling's backing condition). The document's recorded call
/// must still match the stored approval, or the resume is a mismatch. The
/// approval must be a park-retained one: park mode requires the file-backed
/// store, whose `get` returns the approval before and after the decision.
#[allow(dead_code)] // P45 resume endpoint consumes the rehydrate entry points
pub(crate) async fn load_recorded_decisions(
    store: &PendingApprovals,
    doc: &ParkedRun,
) -> Result<(Arc<RecordedDecisions>, Vec<DecisionId>), RehydrateError> {
    let recorded = Arc::new(RecordedDecisions::default());
    let mut decision_ids = Vec::new();
    let mut outstanding = Vec::new();
    // The document's expiry stamp is the run's decision window the 2.6
    // expired row is evaluated against.
    let expires_at = chrono::DateTime::parse_from_rfc3339(&doc.expires_at)
        .map_err(|e| RehydrateError::Document(format!("bad expiry stamp: {e}")))?
        .with_timezone(&chrono::Utc);

    for node in &doc.plan.tasks {
        let crate::orchestration::types::TaskStatus::AwaitingApproval = node.status else {
            continue;
        };
        let Some(pending) = &node.pending else {
            continue;
        };
        for call in pending {
            let parked = store
                .try_parked(&call.decision_id)
                .await
                .map_err(|e| RehydrateError::Store(e.to_string()))?;
            let Some(parked) = parked else {
                return Err(RehydrateError::Mismatch(format!(
                    "store approval {} is missing",
                    call.decision_id
                )));
            };
            // The stored approval must name this run and this checkpoint
            // node: an approval borrowed from another run or task cannot
            // decide this document's call (the 2.6 mismatch row).
            match &parked.request.scope {
                crate::hitl::AgentScope::Worker { run_id, task, .. } => {
                    if run_id.to_string() != doc.run_id {
                        return Err(RehydrateError::Mismatch(format!(
                            "approval {} belongs to run {run_id}, not this run",
                            call.decision_id
                        )));
                    }
                    if task.task_id != node.task_id {
                        return Err(RehydrateError::Mismatch(format!(
                            "approval {} belongs to task {}, not task {}",
                            call.decision_id, task.task_id, node.task_id
                        )));
                    }
                }
                crate::hitl::AgentScope::Single { .. } => {
                    return Err(RehydrateError::Mismatch(format!(
                        "approval {} carries a single-agent scope",
                        call.decision_id
                    )));
                }
                crate::hitl::AgentScope::Coordinator { .. } => {
                    return Err(RehydrateError::Mismatch(format!(
                        "approval {} carries a coordinator scope",
                        call.decision_id
                    )));
                }
            }
            // The approval is single-item by construction (one parked call
            // raises one request); items[0] is the call the human decided on.
            let Some(item) = parked.request.items.first() else {
                return Err(RehydrateError::Mismatch(format!(
                    "approval {} carries no approval item",
                    call.decision_id
                )));
            };
            if item.tool_name != call.tool_name || item.arguments != call.arguments {
                return Err(RehydrateError::Mismatch(format!(
                    "documented call {}({}) does not match the stored approval",
                    call.tool_name, call.decision_id
                )));
            }
            let Some(decision) = store.recorded_decision(&call.decision_id).await else {
                // No decision yet: expired past the window (the 2.6 expired
                // row outranks parked), still parked otherwise — collected
                // so the 409 body can carry every outstanding id.
                if chrono::Utc::now() > expires_at {
                    return Err(RehydrateError::Expired);
                }
                outstanding.push(call.decision_id);
                continue;
            };
            // The key's task id comes from the awaiting node, the tool name
            // and arguments from the store's approval record.
            recorded.push(
                CallKey::new(node.task_id, &item.tool_name, &item.arguments),
                decision,
            );
            decision_ids.push(call.decision_id);
        }
    }
    if !outstanding.is_empty() {
        return Err(RehydrateError::Parked {
            outstanding,
            expires_at,
        });
    }

    Ok((recorded, decision_ids))
}

/// Replace the sentinel tool result for `call_id` in `current_prompt` with
/// `wire` — the result text as the loop delivers it to the model (rig
/// JSON-serializes tool outputs, so a plain string arrives JSON-quoted).
/// Returns whether an entry was replaced; the continuation fails the resume
/// when none was, so no sentinel can survive into the resumed conversation.
pub(crate) fn replace_tool_result(current_prompt: &mut Message, call_id: &str, wire: &str) -> bool {
    let Message::User { content } = current_prompt else {
        return false;
    };
    let mut replaced = false;
    for item in content.iter_mut() {
        if let rig::message::UserContent::ToolResult(tr) = item
            && tr.id == call_id
        {
            tr.content =
                rig::OneOrMany::one(rig::message::ToolResultContent::text(wire.to_string()));
            replaced = true;
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalRequest,
        PROTOCOL_VERSION, ParkedApproval,
    };
    use crate::orchestration::park::document::{ParkedPlan, ParkedTaskNode, SCHEMA_VERSION};
    use crate::orchestration::types::TaskStatus;

    fn parked_run(pending: Vec<PendingCall>) -> ParkedRun {
        ParkedRun {
            schema_version: SCHEMA_VERSION,
            session_id: Some("sess".to_string()),
            run_id: "0191e8c0-aaaa-7000-8000-00000000c0de".to_string(),
            parked_at: "2026-09-02T14:00:00+00:00".to_string(),
            expires_at: (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
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
            .resolve(&decision_id, ApprovalDecision::Approved)
            .await
            .unwrap();

        // The document's copy disagrees on the arguments: the mismatch row
        // refuses the divergence before any entry is built.
        let mismatched = parked_run(vec![pending_call(
            decision_id,
            serde_json::json!({ "namespace": "stage" }),
        )]);
        let err = load_recorded_decisions(&registry, &mismatched)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match the stored approval"),
            "got: {err}"
        );

        // The matching shape: the entry consumes at the resume gate.
        let doc = parked_run(vec![pending_call(decision_id, args.clone())]);
        let (recorded, ids) = load_recorded_decisions(&registry, &doc).await.unwrap();
        assert_eq!(ids, vec![decision_id]);
        assert_eq!(
            recorded.take(&CallKey::new(3, "kubectl_apply", &args)),
            Some(ApprovalDecision::Approved),
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
        )
        .await
        .unwrap_err();
        match err {
            RehydrateError::Parked { outstanding, .. } => {
                assert_eq!(outstanding, vec![undecided]);
            }
            other => panic!("expected Parked with the outstanding id, got: {other}"),
        }

        // The same undecided call past the document's expiry is the expired
        // row.
        let mut doc = parked_run(vec![pending_call(undecided, args)]);
        doc.expires_at = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        let err = load_recorded_decisions(&registry, &doc).await.unwrap_err();
        assert!(err.to_string().contains("expired"), "got: {err}");
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
            .resolve(&decision_id, ApprovalDecision::Approved)
            .await
            .unwrap();

        let doc = parked_run(vec![pending_call(decision_id, args.clone())]);
        let (recorded, ids) = load_recorded_decisions(&registry, &doc).await.unwrap();
        assert_eq!(ids, vec![decision_id]);
        assert_eq!(
            recorded.take(&CallKey::new(3, "kubectl_apply", &args)),
            Some(ApprovalDecision::Approved)
        );
    }

    /// The sentinel replacement: matching call ids swap in the wire result;
    /// an absent call id reports false so the continuation fails instead of
    /// shipping a sentinel back to the model.
    #[test]
    fn replace_tool_result_swaps_only_the_matching_call() {
        let mut prompt = Message::User {
            content: rig::OneOrMany::many(vec![
                rig::message::UserContent::Text(rig::message::Text {
                    text: "context".to_string(),
                }),
                rig::message::UserContent::ToolResult(rig::message::ToolResult {
                    id: "call_a".to_string(),
                    call_id: None,
                    content: rig::OneOrMany::one(rig::message::ToolResultContent::text("sentinel")),
                }),
                rig::message::UserContent::ToolResult(rig::message::ToolResult {
                    id: "call_b".to_string(),
                    call_id: None,
                    content: rig::OneOrMany::one(rig::message::ToolResultContent::text("sentinel")),
                }),
            ])
            .unwrap(),
        };

        assert!(replace_tool_result(&mut prompt, "call_a", "\"applied\""));
        let Message::User { content } = &prompt else {
            unreachable!()
        };
        for item in content.iter() {
            let rig::message::UserContent::ToolResult(tr) = item else {
                continue;
            };
            let text = tr
                .content
                .iter()
                .map(|c| match c {
                    rig::message::ToolResultContent::Text(t) => t.text.clone(),
                    rig::message::ToolResultContent::Image(_) => "[image]".to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            if tr.id == "call_a" {
                assert_eq!(text, "\"applied\"");
            } else {
                assert_eq!(text, "sentinel", "siblings are untouched");
            }
        }

        assert!(
            !replace_tool_result(&mut prompt, "call_missing", "\"x\""),
            "an unknown call id must not silently succeed"
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

        let handle = Arc::new(ResumingDocumentHandle::open(&parked_path).await.unwrap());
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
    }

    /// A missing document opens as NotFound — the section 2.6 "not found"
    /// row.
    #[tokio::test]
    async fn open_reports_not_found_for_a_missing_document() {
        let dir = tempfile::tempdir().unwrap();
        let err = ResumingDocumentHandle::open(&dir.path().join("absent.json"))
            .await
            .unwrap_err();
        assert!(matches!(err, RehydrateError::NotFound));
    }
}
