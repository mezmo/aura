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
    PROTOCOL_VERSION, ParkedApproval, PendingApprovals, ResolvedDecision,
};
use crate::orchestration::test_rig::{
    ECHO_TOOL_RESULT, FreeformArgs, RecordingTool, ScriptedCompletionModel, ScriptedToolCall,
    ScriptedTurn, WORKER_OVERRIDE_SERIAL, WorkerOverride, echo_tool_result_wire,
    install_worker_overrides, take_worker_override,
};
use crate::orchestration::{
    CallKey, OrchestrationConfig, PendingCall, TaskIdentity, TaskStatus, WorkerConfig,
};
use crate::session_store::ApprovalStore;

use super::super::commit::{config_fingerprint, parked_document_dir, publish};
use super::super::document::{
    PARKED_DOCUMENT_SUFFIX, ParkedPlan, ParkedRun, ParkedTaskNode, SCHEMA_VERSION, load_parked_run,
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
/// The second same-key duplicate call's decision id: identical tool and
/// arguments to the first call's, a distinct decision the human records
/// separately.
const DECISION_2: &str = "0199c0de-4545-7000-8000-000000000043";
/// The second same-key duplicate call's id: the slot its own sentinel
/// occupies and its own R2 pair rides under.
const CALL_ID_2: &str = "call_apply_2";
/// The pivot fixture's second call's decision id.
const PIVOT_DECISION_2: &str = "0199c0de-4545-7000-8000-000000000046";
/// The pivot fixture's second call's call id.
const PIVOT_CALL_ID_2: &str = "call_scale_2";
/// The pending call's arguments, fixed so every expected body is literal.
fn call_args() -> Value {
    json!({ "namespace": "prod" })
}

/// The second call's pending arguments — node B's in the two-node fixture,
/// and the pivot fixture's second call's — fixed, and distinct from the
/// first call's.
fn call_args_b() -> Value {
    json!({ "namespace": "prod", "replicas": 3 })
}
/// The newly gated call a re-parking segment issues.
const NEW_TOOL: &str = "kubectl_delete";
const NEW_CALL_ID: &str = "call_id_0";
/// The rig tool-call id the scripted new gated call carries — and so the
/// call id the park stamps on the fresh pending call (`take_current_call_id`
/// stashes the rig id, not the provider call id) and the key the fresh
/// call's R2 pair rides under on the next resume.
const FRESH_CALL_ID: &str = "call_0";
const FINAL_TEXT: &str = "approved and applied";
/// Node B's decision id — the second awaiting node's pending call, fixed so
/// the two-node fixture's expected store state is literal.
const DECISION_B: &str = "0199c0de-4545-7000-8000-000000000044";
/// A second gated tool, a distinct name matching the same `kubectl_*` gate —
/// node B's tool in the two-node fixture, and the pivot fixture's second
/// call's tool.
const TOOL_B: &str = "kubectl_scale";
/// Node B's pending call id, the slot its sentinel occupies.
const CALL_ID_B: &str = "call_scale_1";
/// Node A's continuation text on the second resume.
const A_DONE: &str = "applied and settled";
/// Node B's continuation text on the second resume.
const B_DONE: &str = "scaled and settled";
/// The park placeholder the gate stamps as a parked call's tool result —
/// mirrored here because the gate keeps its sentinel private; the gate's
/// own tests pin the same wording. A decided resume must replace it, so it
/// may appear nowhere in the resumed context or on the wire.
const PARK_SENTINEL: &str =
    "This tool call is parked pending human approval. It has not run. Do not retry.";
/// The recorded denial's reason, fixed so every expected denial text is literal.
const DENIAL_REASON: &str = "the prod namespace is off limits";
/// The error `FailingTool` fails every invocation with, fixed so the parity
/// frame's expected error rendering is a literal.
const TOOL_FAILURE: &str = "the apply command failed: the cluster is unreachable";
/// A decision window far from any test clock, so the expired/mismatch side
/// does not depend on which clock the consult reads.
const FUTURE_STAMP: &str = "2099-01-01T00:00:00Z";
const PAST_STAMP: &str = "2000-01-01T00:00:00Z";
const REQUEST_ID: &str = "req_golden_p45";

fn decision() -> DecisionId {
    DecisionId::parse(DECISION).expect("golden decision id parses")
}

/// The second same-key duplicate call's decision id.
fn decision_2() -> DecisionId {
    DecisionId::parse(DECISION_2).expect("golden decision id parses")
}

/// The pivot fixture's second call's decision id.
fn decision_pivot_2() -> DecisionId {
    DecisionId::parse(PIVOT_DECISION_2).expect("golden decision id parses")
}

/// Node B's decision id.
fn decision_b() -> DecisionId {
    DecisionId::parse(DECISION_B).expect("golden decision id parses")
}

/// A parked-mode config over a file-backed approval store: the fingerprint
/// every matching document carries, and the worker surface the segment
/// frames' continuation rebuilds.
struct World {
    dir: tempfile::TempDir,
    memory_dir: String,
    /// The file approval store behind the registry, held for the store-side
    /// pins a lifecycle frame reads directly (`list_pending`).
    store: Arc<crate::session_store::FileApprovalStore>,
    registry: PendingApprovals,
    config: AgentRuntimeConfig,
    claims: ResumeClaimTable,
}

fn world() -> World {
    world_over_hitl(|registry| crate::hitl::HitlRuntime {
        patterns: Arc::from([aura_config::GlobPattern::new("kubectl_*").unwrap()]),
        route: Arc::new(crate::hitl::DecisionRoute::Conversational {
            registry: registry.clone(),
            timeout: Duration::from_secs(3600),
        }),
        park_enabled: true,
    })
}

/// Build a world over the caller's HITL runtime: the default world speaks
/// the conversational route; the identity frames swap in a poll-delivery
/// webhook whose `tool_headers_from_response` mapping arms the reify-side
/// identity rule.
fn world_over_hitl(hitl: impl FnOnce(&PendingApprovals) -> crate::hitl::HitlRuntime) -> World {
    let dir = tempfile::tempdir().expect("temp memory root");
    std::fs::create_dir_all(dir.path().join("approvals")).expect("approval dir");
    let store = Arc::new(
        crate::session_store::FileApprovalStore::open(dir.path().join("approvals"))
            .expect("file approval store"),
    );
    let registry = PendingApprovals::with_backend(
        store.clone(),
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
        hitl: Some(hitl(&registry)),
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
        store,
        registry,
        config,
        claims: ResumeClaimTable::new(),
    }
}

/// The identity-rule world: the same store and worker surface over a route
/// whose reify-side rule "approved calls must carry identity" is armed —
/// poll delivery keeps the park seam live (`park_registry` holds), the
/// response mapping demands identity (`requires_identity` is true), and
/// the route is built the production way (`HitlRuntime::from_config`). The
/// webhook host is unreachable and never consulted: a recorded hit
/// short-circuits at the gate consult, so the URL only names the shape.
fn identity_world() -> World {
    world_over_hitl(|registry| {
        let config = aura_config::HitlConfig {
            require_approval: vec![aura_config::GlobPattern::new("kubectl_*").unwrap()],
            park: aura_config::ParkConfig {
                enabled: true,
                bind_identity: false,
            },
            route: aura_config::DecisionRouteConfig::Webhook {
                url: aura_config::WebhookUrl::new("https://approvals.example.com/hook").unwrap(),
                timeout_secs: 3600,
                headers: HashMap::new(),
                headers_from_request: HashMap::new(),
                tool_headers_from_response: crate::approver_headers::tests::mappings(&[(
                    "x-forwarded-user",
                    "x-approver-id",
                )]),
                delivery: aura_config::WebhookDelivery::Poll,
                poll_url: None,
                poll_interval_secs: 10,
                poll_request_timeout_secs: 30,
            },
        };
        crate::hitl::HitlRuntime::from_config(&config, registry, None, None)
    })
}

/// The worker-scoped approval for the document's pending call; `run` selects
/// which run the approval names (the mismatch fixture borrows another run's).
fn worker_approval(decision_id: DecisionId, run: &str) -> ParkedApproval {
    node_approval(decision_id, run, 3, TOOL, &call_args())
}

/// The worker-scoped approval for one checkpoint node's pending call: the
/// node's own task id, tool, and arguments (the two-node fixture's second
/// node differs from the first in all three).
fn node_approval(
    decision_id: DecisionId,
    run: &str,
    task_id: usize,
    tool: &str,
    args: &Value,
) -> ParkedApproval {
    ParkedApproval {
        request: ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: "golden-instance".to_string(),
            decision_id,
            request_id: run_owner_id(RUN),
            scope: AgentScope::Worker {
                run_id: run.parse().expect("golden run id parses"),
                task: TaskIdentity::new(task_id, None),
                session_id: None,
            },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: tool.to_string(),
                arguments: args.clone(),
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

/// Register node B's ticket and record an approval on it.
async fn register_decided_b(world: &World) {
    world
        .registry
        .register_durable(node_approval(decision_b(), RUN, 4, TOOL_B, &call_args_b()))
        .await
        .expect("register node B's approval");
    world
        .registry
        .resolve(&decision_b(), ApprovalDecision::Approved.into())
        .await
        .expect("record node B's approval");
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

/// Register both same-key duplicate tickets — two distinct decision ids
/// over the same task, tool, and arguments — undecided.
async fn register_undecided_duplicate_pair(world: &World) {
    for id in [decision(), decision_2()] {
        world
            .registry
            .register_durable(worker_approval(id, RUN))
            .await
            .expect("register the duplicate-pair approval");
    }
}

/// Register both same-key duplicate tickets and record approvals on
/// them: the fixture the same-key duplicate lifecycle drives.
async fn register_decided_duplicate_pair(world: &World) {
    register_undecided_duplicate_pair(world).await;
    for id in [decision(), decision_2()] {
        world
            .registry
            .resolve(&id, ApprovalDecision::Approved.into())
            .await
            .expect("record the duplicate-pair approval");
    }
}

/// Register the pivot fixture's two tickets — the first call's standard
/// ticket through the shared undecided helper, plus the second call's
/// own, riding the SAME node's task under its distinct tool — and record
/// the given decision on each: the pivot frames choose the pair's
/// verdicts (approve/approve, approve/deny, deny/deny).
async fn register_decided_pivot_pair(
    world: &World,
    first: ApprovalDecision,
    second: ApprovalDecision,
) {
    register_undecided(world).await;
    world
        .registry
        .register_durable(node_approval(
            decision_pivot_2(),
            RUN,
            3,
            TOOL_B,
            &call_args_b(),
        ))
        .await
        .expect("register the pivot pair's second approval");
    world
        .registry
        .resolve(&decision(), first.into())
        .await
        .expect("record the pivot pair's first decision");
    world
        .registry
        .resolve(&decision_pivot_2(), second.into())
        .await
        .expect("record the pivot pair's second decision");
}

/// Register both same-key duplicate tickets over the identity world,
/// recording the FIRST approval with its captured identity and the
/// SECOND without: the positional pre-flight must pair the identity-less
/// approval with the second call, a front-only peek would pass it.
async fn register_duplicate_pair_second_without_identity(world: &World) {
    register_undecided_duplicate_pair(world).await;
    world
        .registry
        .resolve(
            &decision(),
            ResolvedDecision::approved(Some(crate::approver_headers::tests::captured_overrides(
                "x-forwarded-user",
                "tok",
            ))),
        )
        .await
        .expect("record the first duplicate's approval with identity");
    world
        .registry
        .resolve(&decision_2(), ApprovalDecision::Approved.into())
        .await
        .expect("record the second duplicate's approval without identity");
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

/// The failing tool's error as the chain delivers it to the model: the live
/// multi-turn loop renders a tool-server error as its `to_string`, raw text
/// rather than the JSON-quoted form a successful output takes — the
/// tool-result rendering the substitution must mirror for an execution
/// `Err` (sync parity). The prefixes are the tool-server round trip's own
/// (`Toolset error: ` over the toolset's and the server's re-wrapped
/// `ToolCallError: ` layers over the tool's), with the doubling collapsed
/// by `ToolError`'s verbatim-prefix rule.
fn tool_failure_wire() -> String {
    format!("Toolset error: ToolCallError: ToolCallError: ToolCallError: {TOOL_FAILURE}")
}

/// A minimal gated tool under the decided call's name whose invocation
/// always fails: it records the call like `RecordingTool`, then returns an
/// ordinary execution `Err` through the same worker wrapper chain and
/// tool-server path — the staging vehicle for the tool-failure parity
/// frame.
struct FailingTool {
    invocations: Arc<Mutex<Vec<Value>>>,
}

impl FailingTool {
    fn new(invocations: Arc<Mutex<Vec<Value>>>) -> Self {
        Self { invocations }
    }
}

impl rig::tool::Tool for FailingTool {
    const NAME: &'static str = "failing_apply";

    type Error = rig::tool::ToolError;
    type Args = FreeformArgs;
    type Output = String;

    // The registered name is the decided call's tool, so the substitution's
    // invocation resolves this tool exactly as it resolves the real one.
    fn name(&self) -> String {
        TOOL.to_string()
    }

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: self.name(),
            description: "Test stand-in: records the call and fails it.".to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.invocations
            .lock()
            .expect("failing-tool invocation log")
            .push(Value::Object(args.fields));
        Err(rig::tool::ToolError::ToolCallError(
            TOOL_FAILURE.to_string().into(),
        ))
    }
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
    sentinel_prompt_for(CALL_ID)
}

/// The checkpointed prompt carrying the sentinel for the given pending call
/// id — the slot a fill's `replace_tool_result` keys on, one per parked call.
fn sentinel_prompt_for(call_id: &str) -> rig::completion::Message {
    tool_result_prompt(call_id, &tool_wire(PARK_SENTINEL))
}

/// The checkpointed prompt carrying the sentinel for each given pending
/// call id — the slot shape a live park leaves for two calls parked out
/// of one worker turn: one message, one tool result per parked call.
fn sentinel_prompt_for_calls(call_ids: &[&str]) -> rig::completion::Message {
    let wire = tool_wire(PARK_SENTINEL);
    let results = call_ids
        .iter()
        .map(|call_id| {
            rig::message::UserContent::ToolResult(rig::message::ToolResult {
                id: (*call_id).to_string(),
                call_id: None,
                content: rig::OneOrMany::one(rig::message::ToolResultContent::text(wire.clone())),
            })
        })
        .collect::<Vec<_>>();
    rig::completion::Message::User {
        content: rig::OneOrMany::many(results).expect("the prompt carries a tool result"),
    }
}

/// The assistant tool call a checkpointed history carries for one gated
/// call: keyed by the pending call id — the provider call id did not
/// survive the park — carrying the recorded tool and arguments.
fn assistant_tool_call(call_id: &str, tool: &str, args: &Value) -> rig::message::AssistantContent {
    rig::message::AssistantContent::ToolCall(rig::message::ToolCall {
        id: call_id.to_string(),
        call_id: None,
        function: rig::message::ToolFunction {
            name: tool.to_string(),
            arguments: args.clone(),
        },
        signature: None,
        additional_params: None,
    })
}

/// The assistant turn carrying the given tool calls: the history message a
/// live park captures for the completion that issued the gated calls — one
/// message, one tool call per call that completion issued.
fn tool_call_turn(calls: Vec<rig::message::AssistantContent>) -> rig::completion::Message {
    rig::completion::Message::Assistant {
        id: None,
        content: rig::OneOrMany::many(calls).expect("the turn carries a tool call"),
    }
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

/// The two-node checkpoint: node A (task 3, the standard sentinel fixture)
/// plus a second awaiting node B (task 4) on the same worker, with its own
/// decision id, call id, a distinct `kubectl_*` tool, its own fixed
/// arguments, and its own sentinel prompt — the fixture the consumed-subset
/// lifecycle drives across two resumes.
fn two_node_sentinel_document(world: &World) -> ParkedRun {
    let mut document = sentinel_document(world);
    document.plan.tasks.push(ParkedTaskNode {
        task_id: 4,
        description: "Gated scale".to_string(),
        dependencies: vec![],
        worker: Some("operations".to_string()),
        rationale: String::new(),
        status: TaskStatus::AwaitingApproval,
        result: None,
        error: None,
        failure_category: None,
        attempt: Some(1),
        history: Some(vec![rig::completion::Message::user("scale it")]),
        current_prompt: Some(sentinel_prompt_for(CALL_ID_B)),
        pending: Some(vec![PendingCall {
            decision_id: decision_b(),
            tool_name: TOOL_B.to_string(),
            arguments: call_args_b(),
            call_id: CALL_ID_B.to_string(),
        }]),
    });
    document
}

/// The same-key duplicate-call checkpoint: one awaiting node (task 3)
/// holding TWO pending calls with identical tool and arguments but
/// distinct call ids and decision ids — the shape one worker turn leaves
/// when it issues the same gated call twice. The history carries the
/// assistant turn that issued BOTH calls (the genuine same-completion
/// shape the producer can write: one message, one tool call per gated
/// call that completion issued), and the current_prompt carries both
/// sentinel slots, one per call id.
fn duplicate_key_document(world: &World) -> ParkedRun {
    let mut document = parked_document(FUTURE_STAMP, matching_fingerprint(world), None, Vec::new());
    let node = document
        .plan
        .tasks
        .first_mut()
        .expect("the skeleton carries one awaiting node");
    node.history = Some(vec![
        rig::completion::Message::user("apply it"),
        tool_call_turn(vec![
            assistant_tool_call(CALL_ID, TOOL, &call_args()),
            assistant_tool_call(CALL_ID_2, TOOL, &call_args()),
        ]),
    ]);
    node.current_prompt = Some(sentinel_prompt_for_calls(&[CALL_ID, CALL_ID_2]));
    node.pending = Some(vec![
        PendingCall {
            decision_id: decision(),
            tool_name: TOOL.to_string(),
            arguments: call_args(),
            call_id: CALL_ID.to_string(),
        },
        PendingCall {
            decision_id: decision_2(),
            tool_name: TOOL.to_string(),
            arguments: call_args(),
            call_id: CALL_ID_2.to_string(),
        },
    ]);
    document
}

/// The pivot checkpoint (the Gate M deny-leg producer shape): one
/// awaiting node (task 3) carrying TWO pending calls on DIFFERENT tools —
/// the standard first call plus the variant call the re-driven worker
/// pivoted to after the park — its own decision, recorded separately from
/// the first call's — which gated and parked too. The history
/// captures the turn that issued the FIRST call only, and the
/// current_prompt carries the sentinel for the FIRST call only: the live
/// park never writes a sentinel slot for a pending call whose tool call
/// the snapshot missed, so the second call's outcome has no checkpointed
/// placeholder to replace — the reconstruction must carry it from the
/// pending record alone.
fn pivot_two_call_document(world: &World) -> ParkedRun {
    let mut document = parked_document(FUTURE_STAMP, matching_fingerprint(world), None, Vec::new());
    let node = document
        .plan
        .tasks
        .first_mut()
        .expect("the skeleton carries one awaiting node");
    node.history = Some(vec![
        rig::completion::Message::user("apply it"),
        tool_call_turn(vec![assistant_tool_call(CALL_ID, TOOL, &call_args())]),
    ]);
    node.current_prompt = Some(sentinel_prompt());
    node.pending = Some(vec![
        PendingCall {
            decision_id: decision(),
            tool_name: TOOL.to_string(),
            arguments: call_args(),
            call_id: CALL_ID.to_string(),
        },
        PendingCall {
            decision_id: decision_pivot_2(),
            tool_name: TOOL_B.to_string(),
            arguments: call_args_b(),
            call_id: PIVOT_CALL_ID_2.to_string(),
        },
    ]);
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

/// Stage the tombstone write's failure: a read-only leftover at the temp
/// path `append_executed_and_publish` writes before renaming onto the
/// resuming document, so the temp open fails `EACCES` and the tombstone
/// publish faults. Directory permissions alone cannot stage this — the
/// write tightens its parent to owner-writable first (`private_dir`) — so
/// the read-only leftover is the filesystem-permission vehicle that holds
/// on macOS and Linux alike.
fn stage_unwritable_tombstone_tmp(world: &World) {
    let tmp = resuming_document_path(world)
        .with_file_name(format!(".{RUN}{RESUMING_DOCUMENT_SUFFIX}.tmp"));
    std::fs::write(&tmp, b"read-only leftover").expect("stage the tombstone temp leftover");
    let mut permissions = std::fs::metadata(&tmp)
        .expect("the staged leftover states")
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&tmp, permissions).expect("make the leftover read-only");
}

/// Load the resuming document as the segment left it on disk — the surface
/// the tombstone assertions read (the once-only evidence the interrupted
/// row keys on, and its absence on the pre-tombstone faults).
async fn resuming_document(world: &World) -> ParkedRun {
    load_parked_run(&resuming_document_path(world))
        .await
        .expect("the claimed run's resuming document reads")
}

/// The diagnostic of a segment fault — the one `SegmentError` variant every
/// fault frame pins its own row's message on. The exhaustive match fails to
/// compile the day a second variant lands, so the pins get re-examined then.
fn continuation_diagnostic(fault: &SegmentError) -> &Diagnostic {
    match fault {
        SegmentError::Continuation(diagnostic) => diagnostic,
    }
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
    decided_call_turn_for(CALL_ID, TOOL, &call_args())
}

/// The pair's call turn keyed by an arbitrary pending call id — the shape a
/// RE-parked call's pair rides under on the next resume, where the recorded
/// call id is the park's own stamp, not the original fixture's.
fn decided_call_turn_for(call_id: &str, tool: &str, args: &Value) -> Value {
    json!({
        "role": "assistant",
        "id": null,
        "content": [
            {
                "id": call_id,
                "call_id": null,
                "function": { "name": tool, "arguments": args },
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
    decided_result_turn_for(CALL_ID, wire)
}

/// The pair's result turn keyed by an arbitrary pending call id.
fn decided_result_turn_for(call_id: &str, wire: &str) -> Value {
    json!({
        "role": "user",
        "content": [
            {
                "type": "toolresult",
                "id": call_id,
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

/// The ids of every tool result the reconstructed context carries, in
/// context order — the outcomes the model receives, keyed by pending call
/// id. The pivot frames pin the exact sequence: no extras, no duplicates,
/// no missing ids, in document order.
fn context_tool_result_ids(context: &rig::OneOrMany<rig::completion::Message>) -> Vec<String> {
    let mut ids = Vec::new();
    for message in context.iter() {
        if let rig::completion::Message::User { content } = message {
            for item in content.iter() {
                if let rig::message::UserContent::ToolResult(tool_result) = item {
                    ids.push(tool_result.id.clone());
                }
            }
        }
    }
    ids
}

/// Assert one pending call's outcome pairing in the reconstructed context
/// the continuation streams from: the model must receive exactly one tool
/// result keyed to this call's OWN id, carrying this call's outcome
/// verbatim, and an assistant tool call keyed to the same id must precede
/// it — the pairing invariant the context builder normalizes (no orphaned
/// results, no missing calls; providers reject a result without its
/// call). The paired call must also name the pending call's tool and carry
/// its recorded arguments, so a builder cannot pair a result with the
/// wrong call.
fn assert_paired_outcome(
    context: &rig::OneOrMany<rig::completion::Message>,
    call_id: &str,
    tool: &str,
    args: &Value,
    wire: &str,
) {
    let mut calls = Vec::<(usize, String, Value)>::new();
    let mut results = Vec::<(usize, String)>::new();
    let mut ordinal = 0usize;
    for message in context.iter() {
        if let rig::completion::Message::Assistant { content, .. } = message {
            for item in content.iter() {
                if let rig::message::AssistantContent::ToolCall(tool_call) = item
                    && tool_call.id == call_id
                {
                    calls.push((
                        ordinal,
                        tool_call.function.name.clone(),
                        tool_call.function.arguments.clone(),
                    ));
                }
                ordinal += 1;
            }
        } else if let rig::completion::Message::User { content } = message {
            for item in content.iter() {
                if let rig::message::UserContent::ToolResult(tool_result) = item
                    && tool_result.id == call_id
                {
                    let text = tool_result
                        .content
                        .iter()
                        .map(|piece| match piece {
                            rig::message::ToolResultContent::Text(text) => text.text.clone(),
                            _ => String::new(),
                        })
                        .collect::<String>();
                    results.push((ordinal, text));
                }
                ordinal += 1;
            }
        }
    }
    assert_eq!(
        results.len(),
        1,
        "the reconstructed context carries exactly one tool result keyed to {call_id}"
    );
    let (result_at, result_text) = &results[0];
    // The pairing invariant: an assistant tool call keyed to the same id
    // precedes the result. Call ids can appear ahead of the pair point in
    // the captured history too, so the claim is the LAST call ahead of the
    // result — the one this result answers.
    let Some((_, paired_tool, paired_args)) = calls.iter().rfind(|(at, _, _)| at < result_at)
    else {
        panic!(
            "the reconstructed context carries no assistant tool call keyed to \
             {call_id} ahead of its tool result — the builder must pair every \
             result with its call, synthesizing the call the checkpoint's \
             history missed"
        );
    };
    assert_eq!(
        paired_tool, tool,
        "the paired assistant tool call keyed to {call_id} names the pending call's tool"
    );
    assert_eq!(
        paired_args, args,
        "the paired assistant tool call keyed to {call_id} carries the pending call's \
         recorded arguments"
    );
    assert_eq!(
        result_text, wire,
        "the tool result keyed to {call_id} carries this call's own outcome verbatim"
    );
}

/// The pivot frames' shared continuation pins: the segment streamed
/// exactly one continuation request, whose context carries exactly the
/// two pending calls' outcomes — keyed to their own call ids, in document
/// order, each preceded by its matching assistant tool call (the second
/// call's call present only by the builder's synthesis) — with the two
/// expected outcome wires as given; no sentinel survives into the
/// reconstructed context; and completion removed both pivot decisions
/// from the store.
async fn assert_pivot_continuation(
    world: &World,
    requests: &Arc<Mutex<Vec<rig::completion::CompletionRequest>>>,
    first_wire: &str,
    second_wire: &str,
) {
    let recorded = requests.lock().expect("scripted-model request log").clone();
    assert_eq!(
        recorded.len(),
        1,
        "the continuation streams exactly one request"
    );
    let context = &recorded[0].chat_history;
    assert_eq!(
        context_tool_result_ids(context),
        [CALL_ID, PIVOT_CALL_ID_2],
        "the model receives exactly the two pending calls' outcomes, keyed to \
         their own call ids, in document order"
    );
    assert_paired_outcome(context, CALL_ID, TOOL, &call_args(), first_wire);
    assert_paired_outcome(
        context,
        PIVOT_CALL_ID_2,
        TOOL_B,
        &call_args_b(),
        second_wire,
    );
    let serialized_context =
        serde_json::to_value(context).expect("the continuation context serializes");
    assert!(
        !serialized_context.to_string().contains(PARK_SENTINEL),
        "the placeholder does not survive into the reconstructed context"
    );
    for id in [decision(), decision_pivot_2()] {
        assert!(
            world
                .registry
                .try_parked(&id)
                .await
                .expect("the store reads")
                .is_none(),
            "completion removes both pivot decisions from the store"
        );
    }
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
    // The decided tool's recording registration and the sentinel prompt are
    // the board-owner repair (logged on the card): without them the
    // substitution prelude would fault for fixture reasons — a missing
    // ToolResult slot to replace, a missing tool to invoke. The frame pins
    // the run-id binding only; execution assertions live elsewhere.
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
            ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                .with_call_id(NEW_CALL_ID),
        ])]),
        extra_tools: vec![
            Box::new(RecordingTool::new(apply_invocations).with_name(TOOL)),
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

/// The two-node consumed-subset lifecycle (fix-contract steps 6 and 8): a
/// checkpoint with TWO awaiting nodes, both decided. Resume 1 drives node A
/// only — its decided call executes once through the substitution, the
/// continuation's new gated call re-parks, and the segment returns Parked at
/// the first re-park, so node B is not driven. The re-park must remove ONLY
/// the actually-consumed subset, after the commit published: node A's
/// original decision is gone from the store, node B's decided ticket
/// survives untouched, and exactly one fresh undecided ticket exists under
/// the original bound run id. Resume 2, over the re-published checkpoint,
/// drives both nodes — node A's fresh call executes once through the
/// substitution and completes, then node B's decided call executes exactly
/// once with its own arguments and completes. The completed turns carry
/// each decided call's R2 outcome pair in segment order, keyed by its own
/// original call id; completion removes the fresh and sibling tickets
/// together; the placeholder appears nowhere.
#[tokio::test]
async fn consumed_subset_re_park_preserves_the_sibling_and_completes_on_the_second_resume() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    let scale_invocations = Arc::new(Mutex::new(Vec::new()));
    // Overrides install per resume, not up front: the queue is take-once and
    // process-global, so a frame that fails mid-lifecycle must not leak the
    // resumes it never drove into the next consumer's builds.
    install_worker_overrides(vec![
        // Resume 1, node A: the continuation issues a new gated call.
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![
                Box::new(RecordingTool::new(apply_invocations.clone()).with_name(TOOL)),
                Box::new(RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL)),
            ],
        },
    ]);
    register_decided(&world).await;
    register_decided_b(&world).await;
    publish_document(&world, &two_node_sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided two-node run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("node A's continuation re-parks and ends the first segment");
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            1,
            "node A's decided call executes exactly once before the re-park; zero \
             invocations recorded / node A's decision never consumed: the substitution \
             prelude does not exist"
        );
        assert_eq!(
            apply_log[0].arguments,
            call_args(),
            "the single invocation carries the recorded call's arguments"
        );
    }
    let blocking = match segment {
        SegmentResult::Parked { blocking, .. } => blocking,
        other => panic!("expected the first segment to re-park, got {other:?}"),
    };
    // The fresh entry is the newly gated tool's; a retained decided sibling
    // may ride the same list (the commit's refreshed pending keeps decided
    // calls for the resume consult), so the shape audit is by tool name and
    // the outstanding set is pinned on the store below, not on the wire.
    let fresh: Vec<_> = blocking
        .as_slice()
        .iter()
        .filter(|entry| entry.tool.as_ref() == NEW_TOOL)
        .collect();
    assert_eq!(
        fresh.len(),
        1,
        "exactly one fresh blocking entry names the newly gated tool: {:?}",
        blocking.as_slice()
    );
    let fresh_decision = fresh[0].decision_id;

    // The consumed-subset pin (step 8): only node A's consumed decision is
    // removed, after the commit published; node B's decided ticket survives
    // untouched.
    assert!(
        world
            .registry
            .try_parked(&decision())
            .await
            .expect("the store reads")
            .is_none(),
        "node A's consumed decision is removed from the store"
    );
    let sibling = world
        .registry
        .try_parked(&decision_b())
        .await
        .expect("the store reads")
        .expect("node B's decided ticket survives the sibling re-park");
    assert_eq!(
        sibling.request.items[0].arguments,
        call_args_b(),
        "node B's ticket is untouched by the sibling re-park"
    );
    assert_eq!(
        world.registry.recorded_decision(&decision_b()).await,
        Some(ResolvedDecision::from(ApprovalDecision::Approved)),
        "node B's approval is still the recorded one"
    );
    // Exactly one fresh undecided ticket, under the original bound run id.
    let undecided = world
        .store
        .list_pending()
        .await
        .expect("the store lists its undecided approvals");
    assert_eq!(
        undecided.len(),
        1,
        "exactly one undecided ticket remains, found ids {:?}",
        undecided
            .iter()
            .map(|ticket| ticket.request.decision_id.to_string())
            .collect::<Vec<_>>()
    );
    let fresh_ticket = &undecided[0];
    assert_eq!(
        fresh_ticket.request.decision_id, fresh_decision,
        "the undecided ticket is the freshly gated call"
    );
    assert_eq!(
        fresh_ticket.request.request_id,
        run_owner_id(RUN),
        "the fresh ticket is registered under the original bound run's owner id"
    );
    let AgentScope::Worker { run_id, task, .. } = &fresh_ticket.request.scope else {
        panic!(
            "the fresh ticket carries a worker scope: {:?}",
            fresh_ticket.request.scope
        )
    };
    assert_eq!(
        &run_id.to_string(),
        RUN,
        "the fresh ticket's scope names the ORIGINAL bound run id"
    );
    assert_eq!(task.task_id, 3, "the fresh ticket names node A's task");

    world
        .registry
        .resolve(&fresh_decision, ApprovalDecision::Approved.into())
        .await
        .expect("record the fresh approval");

    // Resume 2 drives both awaiting nodes in plan order: node A's build
    // first, then node B's — one override per build, in that order.
    install_worker_overrides(vec![
        // Resume 2, node A: the fresh call's substitution, then a final turn.
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(A_DONE)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL),
            )],
        },
        // Resume 2, node B: its decided call's substitution, then a final turn.
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(B_DONE)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(scale_invocations.clone()).with_name(TOOL_B),
            )],
        },
    ]);

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the re-published checkpoint grants the second resume");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the second segment completes");
    let turns = match segment {
        SegmentResult::Completed { turns } => turns,
        other => panic!("expected the second segment to complete, got {other:?}"),
    };
    {
        let fresh_log = fresh_invocations.lock().expect("fresh invocation log");
        assert_eq!(
            fresh_log.len(),
            1,
            "node A's fresh call executes exactly once through the substitution"
        );
        assert_eq!(
            fresh_log[0].arguments,
            json!({ "namespace": "stage" }),
            "the fresh invocation carries the fresh call's arguments"
        );
    }
    {
        let scale_log = scale_invocations.lock().expect("scale invocation log");
        assert_eq!(
            scale_log.len(),
            1,
            "node B's decided call executes exactly once on the second resume"
        );
        assert_eq!(
            scale_log[0].arguments,
            call_args_b(),
            "node B's invocation carries its own recorded arguments"
        );
        assert_eq!(
            scale_log[0].result, ECHO_TOOL_RESULT,
            "node B's invocation returns the tool's real result"
        );
    }
    assert_eq!(
        apply_invocations
            .lock()
            .expect("apply invocation log")
            .len(),
        1,
        "node A's original call is not re-executed on the second resume"
    );
    let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
    assert_eq!(
        serialized,
        json!([
            decided_call_turn_for(FRESH_CALL_ID, NEW_TOOL, &json!({ "namespace": "stage" })),
            decided_result_turn_for(FRESH_CALL_ID, &echo_tool_result_wire()),
            { "role": "assistant", "id": null, "content": [{ "text": A_DONE }] },
            decided_call_turn_for(CALL_ID_B, TOOL_B, &call_args_b()),
            decided_result_turn_for(CALL_ID_B, &echo_tool_result_wire()),
            { "role": "assistant", "id": null, "content": [{ "text": B_DONE }] },
        ]),
        "the completed segment carries each decided call's R2 outcome pair in \
         segment order, keyed by its own original call id, around the per-node \
         final turns"
    );
    assert!(
        !serialized.to_string().contains(PARK_SENTINEL),
        "the placeholder appears nowhere in the serialized turns"
    );
    assert!(
        world
            .registry
            .try_parked(&decision_b())
            .await
            .expect("the store reads")
            .is_none(),
        "node B's ticket is removed on completion"
    );
    assert!(
        world
            .registry
            .try_parked(&fresh_decision)
            .await
            .expect("the store reads")
            .is_none(),
        "the fresh ticket is removed on completion"
    );
}

/// Guard release across the resume chain (fix-contract step 7): the strict
/// guard the substitution arms must be dropped before the continuation
/// streams, so a genuinely new gated call — one absent from the recorded
/// set — re-parks through the LIVE arm and never faults as a strict miss,
/// and re-arms correctly on the next resume, whose substitution consumes
/// the fresh decision and completes. The chain's wire: each resume's turns
/// carry the right outcome pair keyed by the right call id, and no Err
/// surfaces anywhere.
#[tokio::test]
async fn post_substitution_new_call_re_parks_through_the_live_arm_not_a_strict_miss() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    // Per-resume install, as the lifecycle frame documents: no override the
    // frame never drives may leak into another consumer's builds.
    install_worker_overrides(vec![
        // Resume 1: the decided call's substitution, then a new gated call.
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![
                Box::new(RecordingTool::new(apply_invocations.clone()).with_name(TOOL)),
                Box::new(RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL)),
            ],
        },
    ]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect(
            "the continuation re-parks through the live arm; a strict-miss fault \
                 here is the failure this frame exists to catch",
        );
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            1,
            "the decided call executes exactly once before the re-park; zero \
             invocations recorded: the substitution prelude does not exist"
        );
    }
    let (turns, blocking) = match segment {
        SegmentResult::Parked { turns, blocking } => (turns, blocking),
        other => panic!("expected a re-parked segment, got {other:?}"),
    };
    let fresh_decision = blocking
        .as_slice()
        .iter()
        .find(|entry| entry.tool.as_ref() == NEW_TOOL)
        .expect("the new blocking entry names the newly gated tool")
        .decision_id;
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
                            "id": FRESH_CALL_ID,
                            "call_id": NEW_CALL_ID,
                            "function":
                                { "name": NEW_TOOL, "arguments": { "namespace": "stage" } },
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
        "the re-parked segment carries the original pair keyed by the original \
         call id, ahead of the gated assistant turn; the fresh decision id and \
         expiry are location-normalized"
    );

    world
        .registry
        .resolve(&fresh_decision, ApprovalDecision::Approved.into())
        .await
        .expect("record the fresh approval");
    install_worker_overrides(vec![
        // Resume 2: the fresh call's substitution, then a final turn.
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL),
            )],
        },
    ]);
    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the re-published checkpoint grants the second resume");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the re-armed guard consumes the fresh decision; the segment completes");
    let turns = match segment {
        SegmentResult::Completed { turns } => turns,
        other => panic!("expected a completed segment, got {other:?}"),
    };
    {
        let fresh_log = fresh_invocations.lock().expect("fresh invocation log");
        assert_eq!(
            fresh_log.len(),
            1,
            "the fresh call executes exactly once through the substitution"
        );
        assert_eq!(
            fresh_log[0].arguments,
            json!({ "namespace": "stage" }),
            "the single invocation carries the fresh call's arguments"
        );
    }
    let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
    assert_eq!(
        serialized,
        json!([
            decided_call_turn_for(FRESH_CALL_ID, NEW_TOOL, &json!({ "namespace": "stage" })),
            decided_result_turn_for(FRESH_CALL_ID, &echo_tool_result_wire()),
            { "role": "assistant", "id": null, "content": [{ "text": FINAL_TEXT }] },
        ]),
        "the completed segment carries the fresh call's outcome pair keyed by \
         the fresh call's id — resume 1's pair rode the original call id"
    );
    assert!(
        !serialized.to_string().contains(PARK_SENTINEL),
        "the placeholder appears nowhere in the serialized turns"
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

// ====================================================================
// Correction fold, A3: fault and parity rows (fix-contract steps 2-5)
// ====================================================================

/// PARITY — tool-failure-becomes-result-text (fix-contract step 4, second
/// sentence): when the substitution's `call_tool` returns an ordinary
/// execution `Err`, that error becomes the tool-result text for the model
/// — the same raw rendering the live chain's loop delivers — and the
/// segment completes, never faulting. The staging vehicle: `FailingTool`,
/// a gated tool under the decided call's name whose invocation always
/// fails. The completed turns carry the R2 pair keyed by the original call
/// id with the error text as the tool result, ahead of the scripted final
/// turn; no success is fabricated and no placeholder survives.
#[tokio::test]
async fn tool_failure_becomes_result_text_and_the_segment_completes() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(FailingTool::new(invocations.clone()))],
    }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("an execution failure is result text, never a segment fault");
    let SegmentResult::Completed { turns } = segment else {
        panic!("expected a completed segment, got {segment:?}")
    };
    {
        let log = invocations.lock().expect("failing-tool invocation log");
        assert_eq!(
            log.len(),
            1,
            "the decided call executes exactly once and fails; zero invocations \
             recorded: the substitution prelude does not exist"
        );
        assert_eq!(
            log[0],
            call_args(),
            "the single invocation carries the recorded call's arguments"
        );
    }
    let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
    assert_eq!(
        serialized,
        json!([
            decided_call_turn(),
            decided_result_turn(&tool_failure_wire()),
            {
                "role": "assistant",
                "id": null,
                "content": [{ "text": FINAL_TEXT }],
            },
        ]),
        "the completed segment carries the outcome pair holding the error \
         text, keyed by the original call id, ahead of the final turn"
    );
    assert!(
        !serialized.to_string().contains(ECHO_TOOL_RESULT)
            && !serialized.to_string().contains(PARK_SENTINEL),
        "no fabricated success and no placeholder on the wire"
    );
}

/// FAULT — strict-miss fatal (fix-contract step 2, first half): the decided
/// entry missing at substitution time is a fatal `SegmentError` BEFORE any
/// tombstone write or tool invocation. Staging: the grant is taken against
/// the decided fixture, then the decision leaves both surfaces a pre-flight
/// may consult — the store ticket is removed (`registry.remove`) and the
/// grant's in-memory recorded entry is taken — so the substitution peeks
/// nothing for the call.
#[tokio::test]
async fn decided_entry_missing_at_substitution_time_is_fatal_before_the_tombstone() {
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
        .expect("the all-decided run grants against the decided fixture");
    world.registry.remove(&decision()).await;
    assert!(
        grant
            .recorded_decisions()
            .take(&CallKey::new(3, TOOL, &call_args()))
            .is_some(),
        "the staged fixture really held the decided entry"
    );

    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("a decided entry missing at substitution time is fatal");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "resume mismatch: decided call kubectl_apply of task 3 is missing from the \
         recorded set",
        "the fault's diagnostic identifies the strict miss"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the fault precedes any tool invocation"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "no tombstone: the fault precedes the tombstone write"
    );
}

/// FAULT — identity-block fatal, approvals only (fix-contract step 2,
/// second half): a decided APPROVED call whose recorded identity is
/// missing where the route demands identity (the poll-200 capture failed
/// closed) is a fatal `SegmentError` before any tombstone or invocation.
/// Staged over `identity_world` — a poll-delivery webhook route whose
/// response mapping arms the reify-side identity rule — with an approval
/// recorded without identity. The pinned diagnostic is the gate's own
/// wording for the block (`gate.rs`), so the pre-flight and the consult
/// speak one rule.
#[tokio::test]
async fn approved_without_required_identity_is_fatal_before_the_tombstone() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = identity_world();
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
    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("an approval missing the identity the route demands is fatal");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "resume mismatch: approved call is missing required approver identity",
        "the fault's diagnostic identifies the identity block"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the fault precedes any tool invocation"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "no tombstone: the fault precedes the tombstone write"
    );
}

/// The identity-block asymmetry pin: the same identity-demanding route
/// never blocks a DENIAL — denials need no identity, so the recorded
/// denial steers with its normal outcome (Completed, live denial text on
/// the wire keyed by the original call id, zero invocations), never the
/// identity fault. Stands alone from the approval fault above so each
/// fails at its own named point.
#[tokio::test]
async fn denied_without_identity_steers_normally_under_the_identity_route() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = identity_world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
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
        .expect("a denial needs no identity: the segment steers, never faults");
    let SegmentResult::Completed { turns } = segment else {
        panic!("expected a completed segment, got {segment:?}")
    };
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the denied call never executes"
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
        "the denial rides its normal steer outcome — the outcome pair keyed \
         by the original call id, ahead of the final turn"
    );
    assert!(
        !serialized.to_string().contains(PARK_SENTINEL),
        "the park placeholder must not survive a decided resume"
    );
}

/// FAULT — tombstone-failure fatal (fix-contract step 3): a failing
/// `append_executed_and_publish` is fatal before the invocation. Staged by
/// `stage_unwritable_tombstone_tmp` — a read-only leftover at the
/// tombstone write's temp path, the filesystem-permission mechanism that
/// survives the write's own parent-directory tightening. The pinned
/// diagnostic is the proven sequence's own wording, with the standard
/// `EACCES` text.
#[tokio::test]
async fn failing_tombstone_write_is_fatal_before_the_invocation() {
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
    stage_unwritable_tombstone_tmp(&world);
    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("a failing tombstone write is fatal");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "resume tombstone write for call call_apply_1 failed: Permission denied (os error 13)",
        "the fault's diagnostic identifies the tombstone write failure"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the tombstone precedes the invocation, so the fault precedes it too"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "the failed write published no tombstone"
    );
}

/// FAULT — replace-miss fatal (fix-contract step 5): a checkpointed
/// `current_prompt` with NO tool-result slot for the call id makes
/// `replace_tool_result` miss, which is fatal. The fixture is the pre-A1
/// bare-prompt shape — `parked_document`'s `Message::user("tool results")`.
/// Per the contract's ordering (tombstone, then invoke, then replace) the
/// invocation HAS happened and the tombstone IS written: exactly one
/// invocation, and the resuming document's executed list carries the call
/// id. Note this leaves the run in the designed interrupted state — the
/// next resume answers 409 `interrupted` on the once-only evidence.
#[tokio::test]
async fn replace_miss_is_fatal_after_the_tombstone_and_the_invocation() {
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
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, Vec::new()),
    )
    .await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("a prompt with no tool-result slot for the call id is fatal");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "continuation prompt has no tool result for call call_apply_1",
        "the fault's diagnostic identifies the replace miss"
    );
    {
        let log = invocations.lock().expect("tool invocation log");
        assert_eq!(
            log.len(),
            1,
            "exactly one invocation: the tombstone and the invocation both \
             preceded the replace fault"
        );
        assert_eq!(
            log[0].arguments,
            call_args(),
            "the single invocation carries the recorded call's arguments"
        );
    }
    let resuming = resuming_document(&world).await;
    assert_eq!(
        resuming.executed,
        vec![CALL_ID.to_string()],
        "the tombstone IS written: the once-only evidence the interrupted row keys on"
    );
}

// ====================================================================
// Correction fold, Gate A round-1 fixes: the same-key duplicate lifecycle
// and the structural fault rows
// ====================================================================

/// Same-key duplicate lifecycle (Gate A round-1, finding 1): one
/// awaiting node with TWO pending calls — identical tool and arguments,
/// distinct call ids and decision ids — both tickets decided approved.
/// The calls pair with their key's FIFO queue positionally, so both
/// execute exactly once in order, both R2 pairs ride the wire keyed by
/// their OWN call ids, and the mid-segment re-park removes BOTH consumed
/// ids from the store (a front-only consumed derivation under-records
/// the first call's decision and leaks its store row). Resume 2 mirrors
/// the two-resume lifecycle shape: the fresh call drives through the
/// substitution and the segment completes.
#[tokio::test]
async fn same_key_duplicate_calls_execute_once_each_and_a_re_park_removes_both_consumed_ids() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    // Per-resume install, as the lifecycle frames document: the override
    // queue is take-once and process-global.
    install_worker_overrides(vec![
        // Resume 1: the node's continuation issues a new gated call.
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![
                Box::new(RecordingTool::new(apply_invocations.clone()).with_name(TOOL)),
                Box::new(RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL)),
            ],
        },
    ]);
    register_decided_duplicate_pair(&world).await;
    publish_document(&world, &duplicate_key_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided duplicate-pair run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the node's continuation re-parks and ends the first segment");
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            2,
            "both duplicate calls execute exactly once through the substitution"
        );
        assert_eq!(
            apply_log[0].arguments,
            call_args(),
            "the first invocation carries the recorded call's arguments"
        );
        assert_eq!(
            apply_log[1].arguments,
            call_args(),
            "the second invocation carries the same recorded arguments"
        );
    }
    let (turns, blocking) = match segment {
        SegmentResult::Parked { turns, blocking } => (turns, blocking),
        other => panic!("expected a re-parked segment, got {other:?}"),
    };
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
                decided_call_turn_for(CALL_ID_2, TOOL, &call_args()),
                decided_result_turn_for(CALL_ID_2, &echo_tool_result_wire()),
                {
                    "role": "assistant",
                    "id": null,
                    "content": [
                        {
                            "id": FRESH_CALL_ID,
                            "call_id": NEW_CALL_ID,
                            "function":
                                { "name": NEW_TOOL, "arguments": { "namespace": "stage" } },
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
        "the re-parked segment carries BOTH duplicate pairs, each keyed by \
         its own call id, ahead of the gated assistant turn"
    );
    // The re-park removes BOTH consumed ids: the per-call depth derivation
    // must record each duplicate's own decision, never only the first's.
    assert!(
        world
            .registry
            .try_parked(&decision())
            .await
            .expect("the store reads")
            .is_none(),
        "the first duplicate's consumed decision is removed from the store"
    );
    assert!(
        world
            .registry
            .try_parked(&decision_2())
            .await
            .expect("the store reads")
            .is_none(),
        "the second duplicate's consumed decision is removed from the store"
    );
    let undecided = world
        .store
        .list_pending()
        .await
        .expect("the store lists its undecided approvals");
    assert_eq!(
        undecided.len(),
        1,
        "exactly the fresh ticket remains undecided, found ids {:?}",
        undecided
            .iter()
            .map(|ticket| ticket.request.decision_id.to_string())
            .collect::<Vec<_>>()
    );
    let fresh_ticket = &undecided[0];
    assert_eq!(
        fresh_ticket.request.request_id,
        run_owner_id(RUN),
        "the fresh ticket is registered under the original bound run's owner id"
    );
    let AgentScope::Worker { run_id, task, .. } = &fresh_ticket.request.scope else {
        panic!(
            "the fresh ticket carries a worker scope: {:?}",
            fresh_ticket.request.scope
        )
    };
    assert_eq!(
        &run_id.to_string(),
        RUN,
        "the fresh ticket's scope names the ORIGINAL bound run id"
    );
    assert_eq!(
        task.task_id, 3,
        "the fresh ticket names the duplicate node's task"
    );
    let fresh_decision = fresh_ticket.request.decision_id;

    world
        .registry
        .resolve(&fresh_decision, ApprovalDecision::Approved.into())
        .await
        .expect("record the fresh approval");
    install_worker_overrides(vec![
        // Resume 2: the fresh call's substitution, then a final turn.
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL),
            )],
        },
    ]);

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the re-published checkpoint grants the second resume");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the second segment completes");
    let turns = match segment {
        SegmentResult::Completed { turns } => turns,
        other => panic!("expected the second segment to complete, got {other:?}"),
    };
    {
        let fresh_log = fresh_invocations.lock().expect("fresh invocation log");
        assert_eq!(
            fresh_log.len(),
            1,
            "the fresh call executes exactly once through the substitution"
        );
        assert_eq!(
            fresh_log[0].arguments,
            json!({ "namespace": "stage" }),
            "the single invocation carries the fresh call's arguments"
        );
    }
    assert_eq!(
        apply_invocations
            .lock()
            .expect("apply invocation log")
            .len(),
        2,
        "neither duplicate call is re-executed on the second resume"
    );
    let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
    assert_eq!(
        serialized,
        json!([
            decided_call_turn_for(FRESH_CALL_ID, NEW_TOOL, &json!({ "namespace": "stage" })),
            decided_result_turn_for(FRESH_CALL_ID, &echo_tool_result_wire()),
            { "role": "assistant", "id": null, "content": [{ "text": FINAL_TEXT }] },
        ]),
        "the completed segment carries the fresh pair keyed by the fresh \
         call's id — resume 1's pairs rode the duplicate calls' own ids"
    );
    assert!(
        !serialized.to_string().contains(PARK_SENTINEL),
        "the placeholder appears nowhere in the serialized turns"
    );
    for id in [decision(), decision_2(), fresh_decision] {
        assert!(
            world
                .registry
                .try_parked(&id)
                .await
                .expect("the store reads")
                .is_none(),
            "completion leaves none of the duplicate pair's or the fresh ticket's rows"
        );
    }
}

/// FAULT — second-entry-missing (Gate A round-1, finding 1): with two
/// same-key pending calls, a recorded queue one entry short of the
/// pending sequence faults the segment at the missing position — the
/// SECOND call — before any tombstone or invocation, with the
/// strict-miss wording naming the faulting call's tool and task
/// (byte-identical to the single-call frame's literal: the duplicates
/// share tool and task). Staging mirrors the strict-miss frame: the
/// grant is taken against the both-decided fixture, then the queue is
/// left one entry short for two calls — the second ticket leaves the
/// store (`registry.remove`) and one entry leaves the recorded queue
/// (`take` under the shared key).
#[tokio::test]
async fn second_same_key_entry_missing_is_fatal_before_the_tombstone() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    register_decided_duplicate_pair(&world).await;
    publish_document(&world, &duplicate_key_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided duplicate-pair run grants");
    world.registry.remove(&decision_2()).await;
    assert!(
        grant
            .recorded_decisions()
            .take(&CallKey::new(3, TOOL, &call_args()))
            .is_some(),
        "the staged fixture really held the duplicate entries"
    );

    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("a recorded queue too shallow for the pending sequence is fatal");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "resume mismatch: decided call kubectl_apply of task 3 is missing from the \
         recorded set",
        "the fault's diagnostic identifies the strict miss at the second position"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the fault precedes any tool invocation"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "no tombstone: the fault precedes the tombstone write"
    );
}

/// FAULT — second-entry-identity-blocked (Gate A round-1, finding 1,
/// approvals only): with two same-key pending calls under the identity
/// route, an approval recorded WITHOUT identity at the queue's second
/// position faults the segment at that position — a front-only
/// pre-flight would have passed it — with the gate's own identity
/// wording, before any tombstone or invocation.
#[tokio::test]
async fn second_same_key_entry_identity_blocked_is_fatal_before_the_tombstone() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = identity_world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    register_duplicate_pair_second_without_identity(&world).await;
    publish_document(&world, &duplicate_key_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided duplicate-pair run grants");
    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("an identity-less approval at the second position is fatal");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "resume mismatch: approved call is missing required approver identity",
        "the fault's diagnostic identifies the identity block at the second position"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the fault precedes any tool invocation"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "no tombstone: the fault precedes the tombstone write"
    );
}

/// FAULT — pending-absent structural (Gate A round-1, finding 2): an
/// awaiting node whose pending list is ABSENT — the consult skips such
/// nodes, so the run still grants — faults the segment in the driver's
/// seeding loop, beside the attempt/history/prompt checks, before any
/// worker build: a node without pending calls has nothing to
/// substitute, and streaming its checkpointed prompt would carry the
/// stale park placeholder past the prelude. The queued worker override
/// is still queued after the fault — no build consumed it, so no worker
/// streamed — and the scripted model's request log is empty. A node
/// with an EMPTY pending list faults on the same row: the seeding
/// check rejects absent and empty alike.
#[tokio::test]
async fn awaiting_node_without_pending_calls_faults_the_segment_before_any_worker_builds() {
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
    register_decided_b(&world).await;
    // The mixed checkpoint: the malformed awaiting node (task 3, pending
    // absent) rides ahead of the genuinely decided node B.
    let mut document = two_node_sentinel_document(&world);
    let node_a = document
        .plan
        .tasks
        .first_mut()
        .expect("the two-node fixture carries node A first");
    node_a.pending = None;
    publish_document(&world, &document).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the decided sibling grants the malformed run");
    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("an awaiting node without pending calls faults the segment");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "awaiting task 3 carries no pending calls in the checkpoint",
        "the fault's diagnostic identifies the pending-absent node"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "no invocation ran"
    );
    assert!(
        requests
            .lock()
            .expect("scripted-model request log")
            .is_empty(),
        "no worker streamed: the scripted model was never consulted"
    );
    assert!(
        take_worker_override().is_some(),
        "no worker build consumed the queued override: the fault precedes \
         every build"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "no tombstone: the fault precedes the tombstone write"
    );
}

/// FAULT — pending-EMPTY structural (Gate A round-1, finding 2's other
/// half): an awaiting node whose pending list is present but EMPTY faults
/// on the same seeding-loop row as the absent case, before any worker
/// build — a node with nothing to substitute must never stream its
/// checkpointed prompt.
#[tokio::test]
async fn awaiting_node_with_an_empty_pending_list_faults_before_any_worker_builds() {
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
    register_decided_b(&world).await;
    // The mixed checkpoint: the malformed awaiting node (task 3, pending
    // present but empty) rides ahead of the genuinely decided node B.
    let mut document = two_node_sentinel_document(&world);
    let node_a = document
        .plan
        .tasks
        .first_mut()
        .expect("the two-node fixture carries node A first");
    node_a.pending = Some(Vec::new());
    publish_document(&world, &document).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the decided sibling grants the malformed run");
    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("an awaiting node with an empty pending list faults the segment");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "awaiting task 3 carries an empty pending list in the checkpoint",
        "the fault's diagnostic identifies the pending-empty node"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "no invocation ran"
    );
    assert!(
        requests
            .lock()
            .expect("scripted-model request log")
            .is_empty(),
        "no worker streamed: the scripted model was never consulted"
    );
    assert!(
        take_worker_override().is_some(),
        "no worker build consumed the queued override: the fault precedes \
         every build"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "no tombstone: the fault precedes the tombstone write"
    );
}

// ====================================================================
// Reconstruction direction (R5, ruled 2026-09-12): the pivot frames over
// the Gate M deny-leg producer shape. They pin the post-reconstruction
// spec and stay red on the fold's replace-miss fatal until Stage 3
// lands the context builder.
// ====================================================================

/// The pivot shape, both calls approved: the reconstruction must drive
/// BOTH gated calls from the pending records — the sentinel-bearing
/// first call AND the slotless second — executing each exactly once in
/// document order, and complete the segment with no sentinel text
/// surviving into the continuation. Each outcome rides in the
/// reconstructed context keyed to its OWN call id, preceded by its
/// matching assistant tool call — the second call's assistant call is
/// present only by the builder's synthesis (the checkpoint's history
/// never captured it).
#[tokio::test]
async fn pivot_approved_pair_executes_once_each_in_document_order_and_completes() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    // One shared invocation log across both tools: the entry order over
    // the distinct arguments pins document order.
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]);
    let requests = model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model,
        extra_tools: vec![
            Box::new(RecordingTool::new(invocations.clone()).with_name(TOOL)),
            Box::new(RecordingTool::new(invocations.clone()).with_name(TOOL_B)),
        ],
    }]);
    register_decided_pivot_pair(
        &world,
        ApprovalDecision::Approved,
        ApprovalDecision::Approved,
    )
    .await;
    publish_document(&world, &pivot_two_call_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided pivot run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the pivot segment completes");
    let turns = match segment {
        SegmentResult::Completed { turns } => turns,
        other => panic!("expected a completed segment, got {other:?}"),
    };
    {
        let log = invocations.lock().expect("tool invocation log");
        assert_eq!(
            log.len(),
            2,
            "both pivot calls execute exactly once, in document order"
        );
        assert_eq!(
            log[0].arguments,
            call_args(),
            "the first invocation is the first (sentinel-bearing) call, with its \
             recorded arguments"
        );
        assert_eq!(
            log[1].arguments,
            call_args_b(),
            "the second invocation is the second (slotless) call, reconstructed \
             from the pending record alone"
        );
    }
    let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
    assert!(
        !serialized.to_string().contains(PARK_SENTINEL),
        "the placeholder appears nowhere in the serialized turns"
    );
    assert_pivot_continuation(
        &world,
        &requests,
        &echo_tool_result_wire(),
        &echo_tool_result_wire(),
    )
    .await;
}

/// The pivot shape, first call approved and second denied: the approved
/// call executes exactly once, the denied call never executes, and the
/// live denial text with its reason is what the model sees for the
/// second call — reconstructed into the continuation with no
/// checkpointed slot to replace. The denial rides in the tool result
/// keyed to the SECOND call's own id, preceded by its synthesized
/// assistant tool call — not merely somewhere in the context. The
/// segment completes.
#[tokio::test]
async fn pivot_approve_then_deny_executes_only_the_approved_call_and_steers_the_second() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]);
    let requests = model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model,
        extra_tools: vec![
            Box::new(RecordingTool::new(invocations.clone()).with_name(TOOL)),
            Box::new(RecordingTool::new(invocations.clone()).with_name(TOOL_B)),
        ],
    }]);
    register_decided_pivot_pair(
        &world,
        ApprovalDecision::Approved,
        ApprovalDecision::Denied {
            reason: Some(DENIAL_REASON.to_string()),
        },
    )
    .await;
    publish_document(&world, &pivot_two_call_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided pivot run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the pivot segment completes");
    assert!(
        matches!(segment, SegmentResult::Completed { .. }),
        "the segment completes: {segment:?}"
    );
    {
        let log = invocations.lock().expect("tool invocation log");
        assert_eq!(
            log.len(),
            1,
            "exactly one invocation: the approved first call, never the denied second"
        );
        assert_eq!(
            log[0].arguments,
            call_args(),
            "the single invocation carries the approved call's recorded arguments"
        );
    }
    assert_pivot_continuation(
        &world,
        &requests,
        &echo_tool_result_wire(),
        &tool_wire(&denial_text()),
    )
    .await;
}

/// The pivot shape, both calls denied — the Gate M deny leg: ZERO tool
/// invocations, both denial texts delivered to the model, each keyed to
/// its OWN call id (the two denials share their reason, so only
/// id-keyed pairing distinguishes them) and each preceded by its
/// matching assistant tool call — the second call's call present only by
/// the builder's synthesis. The segment completes.
#[tokio::test]
async fn pivot_denied_pair_steers_without_executing_and_completes() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]);
    let requests = model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model,
        extra_tools: vec![
            Box::new(RecordingTool::new(invocations.clone()).with_name(TOOL)),
            Box::new(RecordingTool::new(invocations.clone()).with_name(TOOL_B)),
        ],
    }]);
    register_decided_pivot_pair(
        &world,
        ApprovalDecision::Denied {
            reason: Some(DENIAL_REASON.to_string()),
        },
        ApprovalDecision::Denied {
            reason: Some(DENIAL_REASON.to_string()),
        },
    )
    .await;
    publish_document(&world, &pivot_two_call_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided pivot run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the pivot segment completes");
    assert!(
        matches!(segment, SegmentResult::Completed { .. }),
        "the segment completes: {segment:?}"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "neither denied call executes: zero invocations"
    );
    assert_pivot_continuation(
        &world,
        &requests,
        &tool_wire(&denial_text()),
        &tool_wire(&denial_text()),
    )
    .await;
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
