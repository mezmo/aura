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
    RecordingTool, ScriptedCompletionModel, ScriptedToolCall, ScriptedTurn, WorkerOverride,
    install_worker_overrides,
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

/// The all-decided grant runs the segment to completion: the decided call's
/// continuation produces the final assistant turn, and no blocking set.
#[tokio::test]
async fn all_decided_grant_runs_the_segment_to_completion() {
    let world = world();
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![],
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
        .expect("the segment completes");
    match segment {
        SegmentResult::Completed { turns } => {
            assert_eq!(
                serde_json::to_value(turns.as_slice()).expect("turns serialize"),
                json!([
                    {
                        "role": "assistant",
                        "id": null,
                        "content": [{ "text": FINAL_TEXT }],
                    },
                ]),
                "the completed segment carries the continuation's turns"
            );
        }
        other => panic!("expected a completed segment, got {other:?}"),
    }
}

/// A segment whose continuation issues a newly gated call re-parks: the
/// turns run up to the park, and the blocking set names the new call. The
/// fresh decision id and expiry are location-normalized after an audited
/// shape check; everything else is the literal wire value.
#[tokio::test]
async fn re_park_mid_segment_carries_turns_and_the_new_blocking_entry() {
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
