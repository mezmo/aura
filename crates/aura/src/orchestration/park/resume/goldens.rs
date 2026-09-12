//! Whole-frame golden tests for the resume endpoint's evaluation rows (P45
//! layer 2). Each test assembles one production-reachable checkpoint state
//! and pins the complete 409 body (the exact `Json(row)` value), the
//! detail-less 404 verdict, or the segment result the 200 body projects.
//! `GOLDENS.md` in this directory maps every row to its fixture and records
//! the exclusions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::AgentRuntimeConfig;
use crate::hitl::{
    AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId,
    PROTOCOL_VERSION, ParkedApproval, PendingApprovals,
};
use crate::orchestration::test_rig::{
    ECHO_TOOL_RESULT, RecordingTool, ScriptedCompletionModel, ScriptedToolCall, ScriptedTurn,
    WORKER_OVERRIDE_SERIAL, WorkerOverride, echo_tool_result_wire, install_worker_overrides,
};
use crate::orchestration::{
    OrchestrationConfig, PendingCall, TaskIdentity, TaskStatus, WorkerConfig,
};

use super::super::commit::{config_fingerprint, parked_document_dir, publish};
use super::super::document::{
    PARKED_DOCUMENT_SUFFIX, ParkedPlan, ParkedRun, ParkedTaskNode, SCHEMA_VERSION,
};
use super::super::{RESUMING_DOCUMENT_SUFFIX, run_owner_id};
use super::*;

/// The session path segment every golden requests.
const SESSION: &str = "sess-p45";
/// The run path segment every golden requests.
const RUN: &str = "0199c0de-4545-7000-8000-000000000045";
/// A second run id, for the borrowed-approval mismatch fixture.
const OTHER_RUN: &str = "0199c0de-9999-7000-8000-00000000dead";
/// The pending call's decision id, fixed so every expected body is literal.
const DECISION: &str = "0199c0de-4545-7000-8000-000000000042";
const TOOL: &str = "kubectl_apply";
const CALL_ID: &str = "call_apply_1";
/// The pending call's arguments, fixed so every expected body is literal.
fn call_args() -> Value {
    json!({ "namespace": "prod" })
}
/// The newly gated call a re-parking segment issues.
const NEW_TOOL: &str = "kubectl_delete";
const NEW_CALL_ID: &str = "call_id_0";
const FINAL_TEXT: &str = "approved and applied";
/// The park placeholder the gate stamps as a parked call's tool result —
/// mirrored here because the gate keeps its sentinel private; the gate's
/// own tests pin the same wording. A decided resume must replace it, so it
/// may appear nowhere in the resumed context or on the wire.
const PARK_SENTINEL: &str =
    "This tool call is parked pending human approval. It has not run. Do not retry.";
/// The recorded denial's reason, fixed so every expected denial text is literal.
const DENIAL_REASON: &str = "the prod namespace is off limits";
/// A decision window far from any test clock, so the expired/mismatch side
/// does not depend on which clock the consult reads.
const FUTURE_STAMP: &str = "2099-01-01T00:00:00Z";
const PAST_STAMP: &str = "2000-01-01T00:00:00Z";
const REQUEST_ID: &str = "req_golden_p45";

fn decision() -> DecisionId {
    DecisionId::parse(DECISION).expect("golden decision id parses")
}

/// A parked-mode config over a file-backed approval store: the fingerprint
/// every matching document carries, and the worker surface the segment
/// frames' continuation rebuilds.
struct World {
    dir: tempfile::TempDir,
    memory_dir: String,
    registry: PendingApprovals,
    config: AgentRuntimeConfig,
    claims: ResumeClaimTable,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("temp memory root");
    std::fs::create_dir_all(dir.path().join("approvals")).expect("approval dir");
    let store = crate::session_store::FileApprovalStore::open(dir.path().join("approvals"))
        .expect("file approval store");
    let registry = PendingApprovals::with_backend(
        Arc::new(store),
        Arc::new(crate::session_store::InMemoryEventBus::new()),
    );
    let memory_dir = dir.path().join("memory").to_string_lossy().into_owned();
    std::fs::create_dir_all(&memory_dir).expect("memory dir");
    let mut workers = HashMap::new();
    workers.insert(
        "operations".to_string(),
        WorkerConfig {
            description: "Runs the gated apply".to_string(),
            preamble: "You apply changes with the gated tools.".to_string(),
            mcp_filter: Some(vec![]),
            vector_stores: vec![],
            turn_depth: None,
            llm: None,
            scratchpad: None,
            skills: None,
        },
    );
    let config = AgentRuntimeConfig {
        hitl: Some(crate::hitl::HitlRuntime {
            patterns: Arc::from([aura_config::GlobPattern::new("kubectl_*").unwrap()]),
            route: Arc::new(crate::hitl::DecisionRoute::Conversational {
                registry: registry.clone(),
                timeout: Duration::from_secs(3600),
            }),
            park_enabled: true,
        }),
        memory_dir: Some(memory_dir.clone()),
        session_id: Some(SESSION.to_string()),
        request_id: Some(REQUEST_ID.to_string()),
        orchestration: Some(OrchestrationConfig {
            enabled: true,
            workers,
            ..Default::default()
        }),
        ..AgentRuntimeConfig::default()
    };
    World {
        dir,
        memory_dir,
        registry,
        config,
        claims: ResumeClaimTable::new(),
    }
}

/// The worker-scoped approval for the document's pending call; `run` selects
/// which run the approval names (the mismatch fixture borrows another run's).
fn worker_approval(decision_id: DecisionId, run: &str) -> ParkedApproval {
    ParkedApproval {
        request: ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: "golden-instance".to_string(),
            decision_id,
            request_id: run_owner_id(RUN),
            scope: AgentScope::Worker {
                run_id: run.parse().expect("golden run id parses"),
                task: TaskIdentity::new(3, None),
                session_id: None,
            },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: TOOL.to_string(),
                arguments: call_args(),
                tool_call_intent: None,
            }],
        },
        registered_at: chrono::Utc::now(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        egress_headers: None,
    }
}

/// Register the pending call's ticket, undecided.
async fn register_undecided(world: &World) {
    world
        .registry
        .register_durable(worker_approval(decision(), RUN))
        .await
        .expect("register the parked approval");
}

/// Register the pending call's ticket and record an approval on it.
async fn register_decided(world: &World) {
    register_undecided(world).await;
    world
        .registry
        .resolve(&decision(), ApprovalDecision::Approved.into())
        .await
        .expect("record the approval");
}

/// Register the pending call's ticket and record a reasoned denial on it.
async fn register_denied(world: &World) {
    register_undecided(world).await;
    world
        .registry
        .resolve(
            &decision(),
            ApprovalDecision::Denied {
                reason: Some(DENIAL_REASON.to_string()),
            }
            .into(),
        )
        .await
        .expect("record the denial");
}

/// The live denial text the gate's denial feedback produces for the recorded
/// reason — the wording the gate's own tests pin. A denied call
/// short-circuits with this text, so it is what the resumed worker must see
/// in place of the placeholder.
fn denial_text() -> String {
    format!(
        "Tool call blocked by human approval denial: {DENIAL_REASON}. \
         Do not execute this action."
    )
}

/// A plain tool-output string as the chain delivers it to the model: rig
/// JSON-serializes tool outputs, so a plain string arrives JSON-quoted.
fn tool_wire(text: &str) -> String {
    serde_json::to_string(text).expect("a plain string serializes")
}

/// The checkpointed worker prompt carrying one tool result for `call_id`: the
/// message shape a live park leaves as the awaiting node's `current_prompt`,
/// and the wire shape the R2 tool-result turn reuses.
fn tool_result_prompt(call_id: &str, wire: &str) -> rig::completion::Message {
    rig::completion::Message::User {
        content: rig::OneOrMany::one(rig::message::UserContent::ToolResult(
            rig::message::ToolResult {
                id: call_id.to_string(),
                call_id: None,
                content: rig::OneOrMany::one(rig::message::ToolResultContent::text(
                    wire.to_string(),
                )),
            },
        )),
    }
}

/// The awaiting node's checkpointed prompt as a live park leaves it: the
/// sentinel tool result for the pending call — the placeholder the
/// substitution prelude must replace before the worker ever streams it.
fn sentinel_prompt() -> rig::completion::Message {
    tool_result_prompt(CALL_ID, &tool_wire(PARK_SENTINEL))
}

/// The standard one-call checkpoint with its sentinel prompt: the document a
/// decided resume drives, over the matching fingerprint. The awaiting node's
/// prompt carries the placeholder keyed by the pending call id, so a fill's
/// `replace_tool_result` has the slot the contract requires.
fn sentinel_document(world: &World) -> ParkedRun {
    let mut document = parked_document(FUTURE_STAMP, matching_fingerprint(world), None, Vec::new());
    let node = document
        .plan
        .tasks
        .first_mut()
        .expect("the skeleton carries one awaiting node");
    node.current_prompt = Some(sentinel_prompt());
    document
}

/// A checkpoint document as a park commit writes it: one awaiting node with
/// its captured conversation, the fixed pending call, and the given window,
/// fingerprint, identity binding, and executed tombstones.
fn parked_document(
    expires_at: &str,
    fingerprint: String,
    identity_hash: Option<String>,
    executed: Vec<String>,
) -> ParkedRun {
    ParkedRun {
        schema_version: SCHEMA_VERSION,
        session_id: Some(SESSION.to_string()),
        run_id: RUN.to_string(),
        parked_at: "2026-09-01T00:00:00Z".to_string(),
        expires_at: expires_at.to_string(),
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
                    decision_id: decision(),
                    tool_name: TOOL.to_string(),
                    arguments: call_args(),
                    call_id: CALL_ID.to_string(),
                }]),
            }],
        },
        executed,
        config_fingerprint: fingerprint,
        identity_hash,
    }
}

async fn publish_document(world: &World, document: &ParkedRun) {
    let dir = parked_document_dir(&world.memory_dir, Some(SESSION));
    publish(document, &dir, RUN)
        .await
        .expect("publish the checkpoint");
}

/// Stage a checkpoint under the resuming name, the on-disk state a dead
/// resume leaves behind: the parked directory it was renamed out of still
/// exists.
async fn stage_resuming_document(world: &World, document: &ParkedRun) {
    let dir = parked_document_dir(&world.memory_dir, Some(SESSION));
    let path = dir.join(format!("{RUN}{RESUMING_DOCUMENT_SUFFIX}"));
    std::fs::create_dir_all(&dir).expect("create the parked directory");
    let bytes = serde_json::to_vec_pretty(document).expect("document serializes");
    tokio::task::spawn_blocking(move || std::fs::write(path, bytes))
        .await
        .expect("stage the resuming document")
        .expect("write the resuming document");
}

fn parked_document_path(world: &World) -> std::path::PathBuf {
    parked_document_dir(&world.memory_dir, Some(SESSION))
        .join(format!("{RUN}{PARKED_DOCUMENT_SUFFIX}"))
}

fn resuming_document_path(world: &World) -> std::path::PathBuf {
    parked_document_dir(&world.memory_dir, Some(SESSION))
        .join(format!("{RUN}{RESUMING_DOCUMENT_SUFFIX}"))
}

fn matching_fingerprint(world: &World) -> String {
    config_fingerprint(&world.config)
}

fn evaluation<'a>(
    world: &'a World,
    bind_identity: bool,
    presented: Option<&'a str>,
) -> ResumeEvaluation<'a> {
    ResumeEvaluation {
        path: ValidatedResumePath::parse(SESSION, RUN).expect("golden path validates"),
        memory_dir: &world.memory_dir,
        config: &world.config,
        store: &world.registry,
        claims: &world.claims,
        bind_identity,
        presented_identity: presented,
        request_id: REQUEST_ID.to_string(),
        now: chrono::Utc::now(),
    }
}

fn entry(decision_id: DecisionId, tool: &str, expires_at: &str) -> Value {
    json!({
        "decision_id": decision_id.to_string(),
        "tool": tool,
        "expires_at": expires_at,
    })
}

/// The assistant tool-call turn R2 prepends per decided call: the decided
/// call re-issued on the wire, keyed by the original call id (the id the
/// checkpoint records on the pending call; the provider's own call id did
/// not survive the park).
fn decided_call_turn() -> Value {
    json!({
        "role": "assistant",
        "id": null,
        "content": [
            {
                "id": CALL_ID,
                "call_id": null,
                "function": { "name": TOOL, "arguments": call_args() },
                "signature": null,
                "additional_params": null,
            },
        ],
    })
}

/// The tool-result turn R2 prepends per decided call: the substitution's
/// outcome in the chain's wire form, keyed by the original call id (the
/// contract's "PendingCall.call_id matches ToolResult.id").
fn decided_result_turn(wire: &str) -> Value {
    json!({
        "role": "user",
        "content": [
            {
                "type": "toolresult",
                "id": CALL_ID,
                "content": [{ "type": "text", "text": wire }],
            },
        ],
    })
}

/// The complete 409 body the refusal must render, extracted from the
/// conflict row the web server serializes.
fn assert_conflict(refusal: ResumeRefusal, expected: Value) {
    match &refusal {
        ResumeRefusal::Conflict(row) => {
            assert_eq!(
                serde_json::to_value(row).expect("conflict row serializes"),
                expected,
                "the whole 409 body"
            );
        }
        other => panic!("expected a conflict row, got {other:?}"),
    }
}

/// Rewrite the freshly-minted blocking entry of a mid-segment re-park to
/// placeholders, auditing shape and occurrence count first: exactly one
/// entry, one UUID decision id, one RFC 3339 stamp.
fn normalize_fresh_parking(body: &mut Value) {
    let Value::Array(entries) = &mut body["blocking"] else {
        panic!("the re-park blocking set is an array: {body}")
    };
    assert_eq!(entries.len(), 1, "exactly one new blocking entry: {body:?}");
    let fresh_entry = &mut entries[0];
    let fresh_id = fresh_entry["decision_id"]
        .as_str()
        .expect("the fresh decision id is a string");
    uuid::Uuid::parse_str(fresh_id).expect("the fresh decision id is a UUID");
    assert!(
        fresh_entry["tool"] == json!(NEW_TOOL),
        "the new entry names the newly gated tool: {fresh_entry}"
    );
    let fresh_expiry = fresh_entry["expires_at"]
        .as_str()
        .expect("the fresh expiry is a string")
        .to_owned();
    chrono::DateTime::parse_from_rfc3339(&fresh_expiry).expect("the fresh expiry is RFC 3339");
    fresh_entry["decision_id"] = json!("<fresh decision id>");
    fresh_entry["expires_at"] = json!("<fresh expiry>");
}

/// The empty-memory-root frame answers the detail-less not-found verdict.
#[tokio::test]
async fn missing_checkpoint_refuses_with_the_document_absent_row() {
    let world = world();
    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("an absent checkpoint refuses");
    assert!(
        matches!(refusal, ResumeRefusal::DocumentAbsent),
        "the document-absent row carries no detail payload: {refusal:?}"
    );
}

/// A stored hash differing from the presented header's hash closes the run
/// with the second detail-less verdict.
#[tokio::test]
async fn identity_hash_mismatch_refuses_with_the_detail_less_row() {
    let world = world();
    let document = parked_document(
        FUTURE_STAMP,
        matching_fingerprint(&world),
        Some("aa".repeat(32)),
        vec![],
    );
    publish_document(&world, &document).await;

    let refusal = evaluate_resume(evaluation(&world, true, Some("eve-presented-key")))
        .await
        .expect_err("a bound run with a foreign hash refuses");
    assert!(
        matches!(refusal, ResumeRefusal::IdentityMismatch),
        "the identity-mismatch row carries no detail payload: {refusal:?}"
    );
}

/// An evaluation holding the grant keeps the claim, and the next evaluation
/// of the same run answers the running row with an empty blocking set.
#[tokio::test]
async fn held_claim_refuses_the_second_evaluation_with_the_running_row() {
    let world = world();
    register_decided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants the first evaluation");
    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the held claim refuses a second evaluation");
    assert_conflict(
        refusal,
        json!({
            "code": "running",
            "detail": "another resume holds this run",
            "blocking": [],
        }),
    );
    drop(grant);
}

/// A checkpoint under the resuming name with executed tombstones answers the
/// interrupted row.
#[tokio::test]
async fn executed_tombstones_refuse_with_the_interrupted_row() {
    let world = world();
    register_undecided(&world).await;
    stage_resuming_document(
        &world,
        &parked_document(
            FUTURE_STAMP,
            matching_fingerprint(&world),
            None,
            vec![CALL_ID.to_string()],
        ),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("a dead resume's tombstones refuse");
    assert_conflict(
        refusal,
        json!({
            "code": "interrupted",
            "detail": "a previous resume died mid-segment; the executed list is non-empty",
            "blocking": [],
        }),
    );
}

/// The interrupted row outranks the expired row when both conditions hold:
/// the stage order, not the document's window, decides.
#[tokio::test]
async fn interrupted_outranks_expired() {
    let world = world();
    register_undecided(&world).await;
    stage_resuming_document(
        &world,
        &parked_document(
            PAST_STAMP,
            matching_fingerprint(&world),
            None,
            vec![CALL_ID.to_string()],
        ),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the tombstoned document refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "interrupted",
            "detail": "a previous resume died mid-segment; the executed list is non-empty",
            "blocking": [],
        }),
    );
}

/// An empty resuming checkpoint is renamed back to its parked name under the
/// claim lock and then evaluated as parked.
#[tokio::test]
async fn empty_resuming_document_renames_back_and_answers_the_parked_row() {
    let world = world();
    register_undecided(&world).await;
    stage_resuming_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;
    assert!(resuming_document_path(&world).exists());
    assert!(!parked_document_path(&world).exists());

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the undecided call refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "parked",
            "detail": "calls still await a decision",
            "blocking": [entry(decision(), TOOL, FUTURE_STAMP)],
        }),
    );
    assert!(
        parked_document_path(&world).exists(),
        "the rename-back restored the parked name"
    );
    assert!(
        !resuming_document_path(&world).exists(),
        "the resuming name is gone after the rename-back"
    );
}

/// Two evaluations racing the rename-back of the same crashed resuming
/// document: the claim lock serializes the renames, the loser's ENOENT
/// proceeds against the already-restored parked name, and both answer the
/// parked row - no fault.
#[tokio::test]
async fn concurrent_rename_back_losers_answer_the_parked_row_not_a_fault() {
    let world = world();
    let _race_gate = world.claims.arm_rename_back_race();
    register_undecided(&world).await;
    stage_resuming_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let (first, second) = tokio::join!(
        evaluate_resume(evaluation(&world, false, None)),
        evaluate_resume(evaluation(&world, false, None)),
    );
    for outcome in [first, second] {
        let refusal = outcome.expect_err("undecided calls answer the parked row");
        assert_conflict(
            refusal,
            json!({
                "code": "parked",
                "detail": "calls still await a decision",
                "blocking": [entry(decision(), TOOL, FUTURE_STAMP)],
            }),
        );
    }
    assert!(
        parked_document_path(&world).exists(),
        "the parked name exists for whichever evaluation renamed it back"
    );
    assert!(
        !resuming_document_path(&world).exists(),
        "the resuming name is gone"
    );
}

/// A parked document whose fingerprint no longer matches the rebuilt config
/// answers the config-changed row and leaves the checkpoint and its ticket
/// untouched.
#[tokio::test]
async fn fingerprint_drift_refuses_with_the_config_changed_row() {
    let world = world();
    register_undecided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, "b4".repeat(32), None, vec![]),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("a drifted fingerprint refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "config_changed",
            "detail": "configuration changed since the run parked",
            "blocking": [],
        }),
    );
    assert!(
        parked_document_path(&world).exists(),
        "a fingerprint refusal leaves the parked document in place"
    );
    assert!(
        world
            .registry
            .try_parked(&decision())
            .await
            .expect("the store reads")
            .is_some(),
        "a fingerprint refusal sweeps no approval ticket"
    );
}

/// A ticket naming another run cannot decide this document's calls: the
/// mismatch row, whose detail is the consult's diagnostic prose.
#[tokio::test]
async fn approval_of_another_run_refuses_with_the_mismatch_row() {
    let world = world();
    world
        .registry
        .register_durable(worker_approval(decision(), OTHER_RUN))
        .await
        .expect("register the borrowed approval");
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("a borrowed approval refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "mismatch",
            "detail": format!(
                "approval {DECISION} belongs to run {OTHER_RUN}, not this run"
            ),
            "blocking": [],
        }),
    );
}

/// A missing ticket inside the window is the mismatch row, with an empty
/// blocking set (the narrow reading).
#[tokio::test]
async fn missing_ticket_inside_the_window_refuses_with_the_mismatch_row() {
    let world = world();
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("a vanished ticket refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "mismatch",
            "detail": format!("store approval {DECISION} is missing"),
            "blocking": [],
        }),
    );
}

/// The same missing ticket past the window is the expired row, carrying the
/// pre-sweep blocking list re-derived from the document.
#[tokio::test]
async fn missing_ticket_past_the_window_refuses_with_the_expired_row() {
    let world = world();
    publish_document(
        &world,
        &parked_document(PAST_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("a swept ticket past the window refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "expired",
            "detail": "the decision window closed before every pending call was decided",
            "blocking": [entry(decision(), TOOL, PAST_STAMP)],
        }),
    );
}

/// A parked document with its ticket still undecided answers the parked row
/// with the outstanding set.
#[tokio::test]
async fn undecided_calls_answer_the_parked_row_with_the_outstanding_set() {
    let world = world();
    register_undecided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the undecided call refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "parked",
            "detail": "calls still await a decision",
            "blocking": [entry(decision(), TOOL, FUTURE_STAMP)],
        }),
    );
}

/// Two concurrent evaluations of one run admit exactly one grant; the loser
/// answers the running row, whichever side loses.
#[tokio::test]
async fn concurrent_evaluations_admit_one_grant_and_refuse_the_loser_with_running() {
    let world = world();
    register_decided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let (first, second) = tokio::join!(
        evaluate_resume(evaluation(&world, false, None)),
        evaluate_resume(evaluation(&world, false, None)),
    );
    let outcomes = [first, second];
    let grants = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(grants, 1, "exactly one evaluation claims the run");
    for outcome in outcomes {
        if let Err(refusal) = outcome {
            assert_conflict(
                refusal,
                json!({
                    "code": "running",
                    "detail": "another resume holds this run",
                    "blocking": [],
                }),
            );
        }
    }
}

/// The all-decided grant runs the segment to completion: the decided call
/// executes through the substitution, and the completed segment's turns
/// carry its outcome-bearing pair — the assistant tool-call turn plus the
/// tool-result turn holding the real result, keyed by the original call id
/// — ahead of the continuation's final assistant turn, and no blocking set.
#[tokio::test]
async fn all_decided_grant_runs_the_segment_to_completion() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment completes");
    match segment {
        SegmentResult::Completed { turns } => {
            assert_eq!(
                serde_json::to_value(turns.as_slice()).expect("turns serialize"),
                json!([
                    decided_call_turn(),
                    decided_result_turn(&echo_tool_result_wire()),
                    {
                        "role": "assistant",
                        "id": null,
                        "content": [{ "text": FINAL_TEXT }],
                    },
                ]),
                "the completed segment carries the outcome-bearing pair keyed by \
                 the original call id, ahead of the continuation's turns"
            );
        }
        other => panic!("expected a completed segment, got {other:?}"),
    }
}

/// A segment whose continuation issues a newly gated call re-parks: the
/// decided call executes through the substitution and its outcome-bearing
/// pair rides ahead of the gated assistant turn, and the blocking set names
/// the new call. The fresh decision id and expiry are location-normalized
/// after an audited shape check; everything else is the literal wire value.
#[tokio::test]
async fn re_park_mid_segment_carries_turns_and_the_new_blocking_entry() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
            ScriptedToolCall::new("call_0", NEW_TOOL, json!({ "namespace": "stage" }))
                .with_call_id(NEW_CALL_ID),
        ])]),
        extra_tools: vec![
            Box::new(RecordingTool::new(apply_invocations.clone()).with_name(TOOL)),
            Box::new(RecordingTool::new(invocations).with_name(NEW_TOOL)),
        ],
    }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment re-parks");
    let apply_log = apply_invocations.lock().expect("apply invocation log");
    assert_eq!(
        apply_log.len(),
        1,
        "the decided call executes exactly once before the re-park; zero invocations \
         recorded: the substitution prelude does not exist"
    );
    assert_eq!(
        apply_log[0].arguments,
        call_args(),
        "the single invocation carries the recorded call's arguments"
    );
    drop(apply_log);
    match segment {
        SegmentResult::Parked { turns, blocking } => {
            let mut body = json!({
                "turns": serde_json::to_value(turns.as_slice()).expect("turns serialize"),
                "blocking":
                    serde_json::to_value(blocking.as_slice()).expect("blocking serializes"),
            });
            normalize_fresh_parking(&mut body);
            assert_eq!(
                body,
                json!({
                    "turns": [
                        decided_call_turn(),
                        decided_result_turn(&echo_tool_result_wire()),
                        {
                            "role": "assistant",
                            "id": null,
                            "content": [
                                {
                                    "id": "call_0",
                                    "call_id": NEW_CALL_ID,
                                    "function": {
                                        "name": NEW_TOOL,
                                        "arguments": { "namespace": "stage" },
                                    },
                                    "signature": null,
                                    "additional_params": null,
                                },
                            ],
                        },
                    ],
                    "blocking": [
                        {
                            "decision_id": "<fresh decision id>",
                            "tool": NEW_TOOL,
                            "expires_at": "<fresh expiry>",
                        },
                    ],
                }),
            );
        }
        other => panic!("expected a re-parked segment, got {other:?}"),
    }
}

/// The scope-run-id audit on a mid-segment re-park: the fresh ticket the
/// re-parking segment registers must name the ORIGINAL bound run id — the
/// owner id the sweeps key on and the worker scope stamped on the request —
/// so the next resume's consult matches it. This is the pin for the
/// segment's run-id binding: an orchestrator minting a fresh run id would
/// stamp `run:<fresh>` here and fail the frame.
#[tokio::test]
async fn re_park_registers_the_fresh_ticket_under_the_original_bound_run_id() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
            ScriptedToolCall::new("call_0", NEW_TOOL, json!({ "namespace": "stage" }))
                .with_call_id(NEW_CALL_ID),
        ])]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations).with_name(NEW_TOOL),
        )],
    }]);
    register_decided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment re-parks");
    assert!(
        matches!(segment, SegmentResult::Parked { .. }),
        "the freshly gated call re-parks: {segment:?}"
    );

    // The re-park registers exactly one undecided ticket under the run's
    // owner id; the decided original is not in the cleared set.
    let cleared = world.registry.cancel_request(&run_owner_id(RUN)).await;
    assert_eq!(
        cleared.len(),
        1,
        "the re-park registers one fresh ticket under the original run's owner id"
    );
    let fresh = &cleared[0];
    assert_eq!(
        fresh.request.request_id,
        run_owner_id(RUN),
        "the fresh ticket's owner id names the ORIGINAL bound run id"
    );
    let AgentScope::Worker { run_id, task, .. } = &fresh.request.scope else {
        panic!(
            "the fresh ticket carries a worker scope: {:?}",
            fresh.request.scope
        );
    };
    assert_eq!(
        &run_id.to_string(),
        RUN,
        "the fresh ticket's scope names the ORIGINAL bound run id"
    );
    assert_eq!(
        task.task_id, 3,
        "the fresh ticket names the checkpoint node's task"
    );
}

/// A recorded approval executes exactly once through the worker's gated
/// pipeline, and the completed segment's turns carry the outcome-bearing
/// pair — the assistant tool-call turn for the decided call plus the
/// tool-result turn holding the tool's real result, keyed by the original
/// call id — ahead of the continuation's final assistant turn. The park
/// placeholder appears nowhere on the wire.
#[tokio::test]
async fn approved_call_executes_once_and_rides_the_outcome_pair() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment completes");
    match segment {
        SegmentResult::Completed { turns } => {
            let log = invocations.lock().expect("tool invocation log");
            assert_eq!(
                log.len(),
                1,
                "the approved call executes exactly once; zero invocations recorded: \
                 the substitution prelude does not exist"
            );
            assert_eq!(
                log[0].arguments,
                call_args(),
                "the single invocation carries the recorded call's arguments"
            );
            assert_eq!(
                log[0].result, ECHO_TOOL_RESULT,
                "the single invocation returns the tool's real result"
            );
            drop(log);

            let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
            assert_eq!(
                serialized,
                json!([
                    decided_call_turn(),
                    decided_result_turn(&echo_tool_result_wire()),
                    {
                        "role": "assistant",
                        "id": null,
                        "content": [{ "text": FINAL_TEXT }],
                    },
                ]),
                "the completed segment's turns carry the outcome-bearing pair keyed \
                 by the original call id, ahead of the final turn"
            );
            assert!(
                !serialized.to_string().contains(PARK_SENTINEL),
                "the park placeholder must not survive a decided resume"
            );
        }
        other => panic!("expected a completed segment, got {other:?}"),
    }
}

/// A recorded denial steers: the call never executes, the live denial text
/// and its reason ride the continuation context verbatim in place of the
/// placeholder, and the wire carries the outcome-bearing pair holding the
/// live denial text, keyed by the original call id, ahead of the scripted
/// final turn — the worker adapts; no result is fabricated.
#[tokio::test]
async fn denied_call_steers_without_executing_and_rides_the_denial_pair() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]);
    let requests = model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model,
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    register_denied(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment completes");
    match segment {
        SegmentResult::Completed { turns } => {
            assert!(
                invocations.lock().expect("tool invocation log").is_empty(),
                "the denied call never executes"
            );

            let recorded = requests.lock().expect("scripted-model request log").clone();
            assert_eq!(
                recorded.len(),
                1,
                "the continuation is exactly one model turn"
            );
            let context = serde_json::to_value(&recorded[0].chat_history)
                .expect("the continuation context serializes");
            assert_eq!(
                context,
                json!([
                    { "role": "user", "content": [{ "type": "text", "text": "apply it" }] },
                    decided_result_turn(&tool_wire(&denial_text())),
                ]),
                "the worker's context carries the live denial text and its reason \
                 verbatim, in place of the placeholder"
            );

            let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
            assert_eq!(
                serialized,
                json!([
                    decided_call_turn(),
                    decided_result_turn(&tool_wire(&denial_text())),
                    {
                        "role": "assistant",
                        "id": null,
                        "content": [{ "text": FINAL_TEXT }],
                    },
                ]),
                "the denial rides the outcome-bearing pair on the wire, keyed by \
                 the original call id, ahead of the final turn"
            );
            assert!(
                !serialized.to_string().contains(PARK_SENTINEL)
                    && !context.to_string().contains(PARK_SENTINEL),
                "the park placeholder must not survive a decided resume's context \
                 or wire"
            );
        }
        other => panic!("expected a completed segment, got {other:?}"),
    }
}

/// The wire serializers the golden literals embed, calibrated against the
/// implemented types: the blocking-entry object and the rig assistant turn.
#[test]
fn blocking_entry_and_turn_literals_match_the_wire_serializers() {
    let wired = BlockingEntry {
        decision_id: decision(),
        tool: ParkedToolName::new(TOOL),
        expires_at: chrono::DateTime::parse_from_rfc3339(FUTURE_STAMP)
            .expect("golden stamp parses")
            .with_timezone(&chrono::Utc),
    };
    assert_eq!(
        serde_json::to_value(&wired).expect("the entry serializes"),
        entry(decision(), TOOL, FUTURE_STAMP),
        "the blocking-entry wire object the 409 and 200 goldens embed"
    );
    assert_eq!(
        serde_json::to_value(rig::completion::Message::assistant(FINAL_TEXT))
            .expect("the turn serializes"),
        json!({
            "role": "assistant",
            "id": null,
            "content": [{ "text": FINAL_TEXT }],
        }),
        "the segment-turn wire object the 200 goldens embed"
    );
}

/// The wire serializers the R2 outcome-turn and sentinel literals embed,
/// calibrated against the implemented rig types: the decided call's
/// assistant tool-call turn, the tool-result turn over the chain's wire
/// form, the sentinel prompt the decided-resume fixtures stage, and the
/// JSON-quoted wire forms of the sentinel and the live denial text.
#[test]
fn outcome_pair_and_sentinel_literals_match_the_wire_serializers() {
    let call_turn = rig::completion::Message::Assistant {
        id: None,
        content: rig::OneOrMany::one(rig::message::AssistantContent::ToolCall(
            rig::message::ToolCall {
                id: CALL_ID.to_string(),
                call_id: None,
                function: rig::message::ToolFunction {
                    name: TOOL.to_string(),
                    arguments: call_args(),
                },
                signature: None,
                additional_params: None,
            },
        )),
    };
    assert_eq!(
        serde_json::to_value(&call_turn).expect("the call turn serializes"),
        decided_call_turn(),
        "the assistant tool-call turn literal the R2 frames embed"
    );

    let result_turn = tool_result_prompt(CALL_ID, &echo_tool_result_wire());
    assert_eq!(
        serde_json::to_value(&result_turn).expect("the result turn serializes"),
        decided_result_turn(&echo_tool_result_wire()),
        "the tool-result turn literal the R2 frames embed"
    );

    assert_eq!(
        serde_json::to_value(sentinel_prompt()).expect("the sentinel prompt serializes"),
        decided_result_turn(&tool_wire(PARK_SENTINEL)),
        "the sentinel prompt the fixtures stage, on the tool-result wire"
    );
    assert_eq!(
        tool_wire(PARK_SENTINEL),
        "\"This tool call is parked pending human approval. It has not run. Do not retry.\"",
        "the sentinel's wire form is the JSON-quoted string the chain delivers"
    );
    assert_eq!(
        tool_wire(&denial_text()),
        "\"Tool call blocked by human approval denial: the prod namespace is off limits. \
         Do not execute this action.\"",
        "the denial's wire form is the JSON-quoted live denial text"
    );
}
