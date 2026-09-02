//! The parked-run checkpoint document.
//!
//! The document is the durable record of a run stopped at the park verdict.
//! It carries everything a later resume needs to reconstruct the run — the
//! original query, the external and coordinator conversations, the plan with
//! each awaiting node's captured worker conversation, and the config
//! fingerprint — and never carries approval decisions: a decision can only
//! ever be read back from the approval store, never copied into a document.

use std::io;
use std::path::Path;

use rig::completion::Message;
use serde::{Deserialize, Serialize};

use crate::orchestration::types::{
    FailedTaskRecord, FailureCategory, PendingCall, Plan, PlanningResponse, StepInput, TaskState,
    TaskStatus,
};

use super::ParkedTaskRecords;

/// The checkpoint format version this build writes and accepts.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Filename suffix of a published checkpoint: `{run_id}.json`.
pub(crate) const PARKED_DOCUMENT_SUFFIX: &str = ".json";

/// Filename suffix of the document a resume rewrites before continuing:
/// `{run_id}.resuming.json`.
pub(crate) const RESUMING_DOCUMENT_SUFFIX: &str = ".resuming.json";

/// The plan as recorded in a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ParkedPlan {
    /// The goal being addressed.
    pub goal: String,
    /// Original step structure from the coordinator, when the plan was built
    /// from steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<StepInput>>,
    /// The plan's tasks; awaiting nodes carry their park payload.
    pub tasks: Vec<ParkedTaskNode>,
}

/// One task of a checkpoint plan.
///
/// The `status` field selects the shape of the optional fields, mirroring
/// the `Task` wire form: awaiting nodes carry the blocking `attempt`, the
/// `pending` calls, and the captured worker conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ParkedTaskNode {
    pub task_id: usize,
    pub description: String,
    #[serde(default)]
    pub dependencies: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    #[serde(default)]
    pub rationale: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    /// The worker attempt that blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    /// The calls awaiting a human decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<PendingCall>>,
    /// Everything before the final worker turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Message>>,
    /// The final worker turn's aggregated tool-result prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_prompt: Option<Message>,
}

impl ParkedTaskNode {
    /// The node's pending calls when it awaits decisions, else empty.
    #[allow(dead_code)]
    pub fn awaiting(&self) -> &[PendingCall] {
        match (&self.status, &self.pending) {
            (TaskStatus::AwaitingApproval, Some(pending)) => pending,
            _ => &[],
        }
    }
}

/// The parked-run checkpoint document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ParkedRun {
    /// Checkpoint format version.
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub run_id: String,
    /// RFC 3339 timestamp of the park commit.
    pub parked_at: String,
    /// RFC 3339 timestamp after which the run's decisions have expired.
    pub expires_at: String,
    /// The query that started the run.
    pub query: String,
    /// The external chat history the run was started with.
    pub chat_history: Vec<Message>,
    /// The coordinator's conversation across its planning turns.
    pub coordinator_conversation: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_decision: Option<PlanningResponse>,
    pub iteration: usize,
    pub planning_ms: u64,
    #[serde(default)]
    pub failure_history: Vec<FailedTaskRecord>,
    pub plan: ParkedPlan,
    /// Task ids executed since the document was written. A non-empty list
    /// marks a resumed document terminal under either filename.
    #[serde(default)]
    pub executed: Vec<usize>,
    pub config_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_hash: Option<String>,
}

impl ParkedRun {
    /// The awaiting set re-derived from the document alone: one entry per
    /// awaiting node, with its decision ids.
    #[allow(dead_code)]
    pub fn awaiting_decision_ids(&self) -> Vec<String> {
        self.plan
            .tasks
            .iter()
            .flat_map(|t| t.awaiting().iter().map(|c| c.decision_id.to_string()))
            .collect()
    }
}

/// Everything the document builder needs from the run's current state.
#[derive(Debug)]
pub(crate) struct RunStateForPark<'a> {
    pub run_id: &'a str,
    pub session_id: Option<&'a str>,
    pub query: &'a str,
    pub chat_history: &'a [Message],
    pub coordinator_conversation: &'a [Message],
    pub routing_decision: Option<&'a PlanningResponse>,
    pub iteration: usize,
    pub planning_ms: u64,
    pub failure_history: &'a [FailedTaskRecord],
}

/// Build the checkpoint from the run's current state.
///
/// `pending_by_task` narrows each awaiting node to the calls still awaiting
/// a decision (see [`super::commit::refresh_awaiting`]); `expires_at` is the
/// RFC 3339 deadline the refreshed set derived from the approval store.
/// `executed` is empty: the tombstone grows only on a resumed document.
pub(crate) fn build_document(
    state: &RunStateForPark<'_>,
    plan: &Plan,
    records: &ParkedTaskRecords,
    pending_by_task: &std::collections::HashMap<usize, Vec<PendingCall>>,
    expires_at: String,
    config_fingerprint: String,
    identity_hash: Option<String>,
) -> ParkedRun {
    let tasks = plan
        .tasks
        .iter()
        .map(|t| {
            let mut node = ParkedTaskNode {
                task_id: t.id,
                description: t.description.clone(),
                dependencies: t.dependencies.clone(),
                worker: t.worker.clone(),
                rationale: t.rationale.clone(),
                status: TaskStatus::from(&t.state),
                result: None,
                error: None,
                failure_category: None,
                attempt: None,
                pending: None,
                history: None,
                current_prompt: None,
            };
            match &t.state {
                TaskState::AwaitingApproval { .. } => {
                    let record = records.get(&t.id);
                    node.attempt = record.map(|r| r.attempt);
                    node.history = record.map(|r| r.snapshot.history.clone());
                    node.current_prompt = record.map(|r| r.snapshot.current_prompt.clone());
                    node.pending = Some(pending_by_task.get(&t.id).cloned().unwrap_or_default());
                }
                TaskState::Complete { result } => node.result = Some(result.clone()),
                TaskState::Failed { error, category } => {
                    node.error = Some(error.clone());
                    node.failure_category = Some(*category);
                }
                TaskState::Pending | TaskState::Running => {}
            }
            node
        })
        .collect();

    ParkedRun {
        schema_version: SCHEMA_VERSION,
        session_id: state.session_id.map(str::to_string),
        run_id: state.run_id.to_string(),
        parked_at: chrono::Utc::now().to_rfc3339(),
        expires_at,
        query: state.query.to_string(),
        chat_history: state.chat_history.to_vec(),
        coordinator_conversation: state.coordinator_conversation.to_vec(),
        routing_decision: state.routing_decision.cloned(),
        iteration: state.iteration,
        planning_ms: state.planning_ms,
        failure_history: state.failure_history.to_vec(),
        plan: ParkedPlan {
            goal: plan.goal.clone(),
            steps: plan.steps.clone(),
            tasks,
        },
        executed: Vec::new(),
        config_fingerprint,
        identity_hash,
    }
}

/// Load a checkpoint document from `path`, rejecting unknown schema
/// versions. The read runs on the blocking pool.
#[allow(dead_code)]
pub(crate) async fn load_parked_run(path: &Path) -> io::Result<ParkedRun> {
    let display = path.display().to_string();
    let path = path.to_path_buf();
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(&path))
        .await
        .map_err(io::Error::other)??;
    let document: ParkedRun = serde_json::from_slice(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parked run at {display} failed to decode: {e}"),
        )
    })?;
    if document.schema_version != SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "parked run at {display} has schema version {} (expected {SCHEMA_VERSION})",
                document.schema_version
            ),
        ));
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hitl::DecisionId;
    use crate::orchestration::Task;
    use crate::orchestration::park::ParkedTaskRecord;

    const GOLDEN: &str = include_str!("../../../testdata/park/parked_run_v1.json");

    fn pending_call(tool: &str, call_id: &str) -> PendingCall {
        PendingCall {
            decision_id: DecisionId::generate(),
            tool_name: tool.to_string(),
            arguments: serde_json::json!({ "namespace": "prod" }),
            call_id: call_id.to_string(),
        }
    }

    fn park_fixture_state<'a>(
        query: &'a str,
        chat_history: &'a [rig::completion::Message],
    ) -> RunStateForPark<'a> {
        RunStateForPark {
            run_id: "0191e8c0-aaaa-7000-8000-000000000001",
            session_id: Some("sess-1"),
            query,
            chat_history,
            coordinator_conversation: &[],
            routing_decision: None,
            iteration: 2,
            planning_ms: 1500,
            failure_history: &[],
        }
    }

    /// A plan with one completed, one awaiting, and one pending task builds
    /// a document whose awaiting node carries the record's attempt and
    /// captured conversation, whose completed node carries its result, and
    /// whose `executed` list is empty.
    #[test]
    fn build_document_shapes_nodes_by_state() {
        let mut plan = Plan::new("Deploy");
        plan.add_task(Task::new(0, "Facts", "r"));
        plan.add_task(Task::new(1, "Gated apply", "r").with_dependency(0));
        plan.add_task(Task::new(2, "Verify", "r").with_dependency(1));
        plan.get_task_mut(0).unwrap().complete("facts");

        let pending = vec![pending_call("kubectl_apply", "call_7")];
        plan.get_task_mut(1).unwrap().state = TaskState::AwaitingApproval {
            pending: pending.clone(),
        };

        let history = vec![rig::completion::Message::user("apply it")];
        let snapshot_prompt = rig::completion::Message::user("tool results");
        let mut records = ParkedTaskRecords::new();
        records.insert(
            1,
            ParkedTaskRecord {
                attempt: 2,
                snapshot: crate::orchestration::ParkSnapshot {
                    history: history.clone(),
                    current_prompt: snapshot_prompt.clone(),
                },
            },
        );

        let mut pending_by_task = std::collections::HashMap::new();
        pending_by_task.insert(1, pending);

        let chat_history = vec![rig::completion::Message::user("prior turn")];
        let doc = build_document(
            &park_fixture_state("Deploy it", &chat_history),
            &plan,
            &records,
            &pending_by_task,
            "2026-09-02T15:00:00+00:00".to_string(),
            "fingerprint".to_string(),
            None,
        );

        assert_eq!(doc.schema_version, SCHEMA_VERSION);
        assert_eq!(doc.executed, Vec::<usize>::new());
        assert_eq!(doc.query, "Deploy it");
        assert_eq!(doc.iteration, 2);

        let awaiting = &doc.plan.tasks[1];
        assert_eq!(awaiting.status, TaskStatus::AwaitingApproval);
        assert_eq!(awaiting.attempt, Some(2));
        assert_eq!(awaiting.awaiting().len(), 1);
        assert_eq!(
            awaiting
                .history
                .as_deref()
                .map(<[rig::completion::Message]>::len),
            Some(1)
        );
        assert!(awaiting.current_prompt.is_some());

        let completed = &doc.plan.tasks[0];
        assert_eq!(completed.status, TaskStatus::Complete);
        assert_eq!(completed.result.as_deref(), Some("facts"));

        let untouched = &doc.plan.tasks[2];
        assert_eq!(untouched.status, TaskStatus::Pending);
        assert!(untouched.pending.is_none());
    }

    /// The full document serde round-trips: the awaiting set re-derives from
    /// the serialized form alone.
    #[test]
    fn document_serde_roundtrip_rederives_awaiting_set() {
        let mut plan = Plan::new("Deploy");
        plan.add_task(Task::new(0, "Gated apply", "r"));
        let pending = vec![pending_call("kubectl_delete", "call_9")];
        plan.get_task_mut(0).unwrap().state = TaskState::AwaitingApproval {
            pending: pending.clone(),
        };
        let mut records = ParkedTaskRecords::new();
        records.insert(
            0,
            ParkedTaskRecord {
                attempt: 1,
                snapshot: crate::orchestration::ParkSnapshot {
                    history: vec![rig::completion::Message::user("do it")],
                    current_prompt: rig::completion::Message::user("results"),
                },
            },
        );
        let mut pending_by_task = std::collections::HashMap::new();
        pending_by_task.insert(0, pending);

        let chat_history = vec![rig::completion::Message::user("prior turn")];
        let doc = build_document(
            &park_fixture_state("Deploy", &chat_history),
            &plan,
            &records,
            &pending_by_task,
            "2026-09-02T15:00:00+00:00".to_string(),
            "fingerprint".to_string(),
            Some("identity".to_string()),
        );

        let json = serde_json::to_string(&doc).unwrap();
        let back: ParkedRun = serde_json::from_str(&json).unwrap();

        assert_eq!(back.awaiting_decision_ids(), doc.awaiting_decision_ids());
        let node = &back.plan.tasks[0];
        assert_eq!(node.attempt, Some(1));
        assert_eq!(node.awaiting().len(), 1);
        assert_eq!(node.awaiting()[0].tool_name, "kubectl_delete");
        assert_eq!(node.history.as_ref().map(Vec::len), Some(1));
        assert!(node.current_prompt.is_some());
        assert_eq!(back.identity_hash.as_deref(), Some("identity"));
    }

    /// The checked-in golden fixture decodes, re-derives its awaiting set
    /// from disk alone, and re-serializes to the identical JSON — locking
    /// the wire form.
    #[test]
    fn golden_fixture_round_trips() {
        let raw = GOLDEN;
        let doc: ParkedRun = serde_json::from_str(raw).expect("golden fixture decodes");

        assert_eq!(doc.schema_version, SCHEMA_VERSION);
        assert_eq!(doc.run_id, "0191e8c0-aaaa-7000-8000-00000000c0de");
        assert_eq!(
            doc.awaiting_decision_ids(),
            vec!["0191e8c0-bbbb-7000-8000-000000000042".to_string()],
            "the awaiting set re-derives from disk alone"
        );
        assert!(doc.executed.is_empty(), "empty at park");

        let awaiting = doc
            .plan
            .tasks
            .iter()
            .find(|t| t.status == TaskStatus::AwaitingApproval)
            .expect("awaiting node present");
        assert_eq!(awaiting.attempt, Some(1));
        assert_eq!(awaiting.awaiting()[0].tool_name, "kubectl_apply");
        assert_eq!(awaiting.awaiting()[0].call_id, "call_apply_1");
        assert!(awaiting.history.is_some());
        assert!(awaiting.current_prompt.is_some());

        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            value,
            "round-trip must not change the wire form"
        );
    }

    /// The load path reads a checkpoint from disk and rejects unknown
    /// schema versions.
    #[tokio::test]
    async fn load_reads_from_disk_and_rejects_foreign_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parked.json");
        std::fs::write(&path, GOLDEN).unwrap();

        let doc = load_parked_run(&path)
            .await
            .expect("golden loads from disk");
        assert_eq!(doc.awaiting_decision_ids().len(), 1);

        let mut foreign: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        foreign["schema_version"] = serde_json::json!(99);
        std::fs::write(&path, serde_json::to_string(&foreign).unwrap()).unwrap();
        let err = load_parked_run(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("schema version 99"),
            "error names the version: {err}"
        );
    }
}
