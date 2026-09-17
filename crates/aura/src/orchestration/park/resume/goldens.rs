//! Whole-frame golden tests for the resume endpoint's evaluation rows (P45
//! layer 2). Each test assembles one production-reachable checkpoint state
//! and pins the complete 409 body (the exact `Json(row)` value), the
//! detail-less 404 verdict, or the segment result the 200 body projects.
//! `GOLDENS.md` in this directory maps every row to its fixture and records
//! the exclusions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::config::AgentRuntimeConfig;
use crate::hitl::{
    AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId,
    PROTOCOL_VERSION, ParkedApproval, PendingApprovals, ResolvedDecision,
};
use crate::orchestration::test_rig::{
    CoordinatorOverride, ECHO_TOOL_RESULT, FreeformArgs, RecordingTool, ScriptedCompletionModel,
    ScriptedToolCall, ScriptedTurn, WORKER_OVERRIDE_SERIAL, WorkerOverride, echo_tool_result_wire,
    install_coordinator_overrides, install_worker_overrides, take_coordinator_override,
    take_worker_override,
};
use crate::orchestration::types::{FailedTaskRecord, FailureCategory};
use crate::orchestration::{
    CallKey, OrchestrationConfig, PendingCall, TaskIdentity, TaskStatus, WorkerConfig,
};
use crate::session_store::ApprovalStore;

use super::super::commit::{config_fingerprint, parked_document_dir, publish};
use super::super::document::{
    PARKED_DOCUMENT_SUFFIX, ParkedPlan, ParkedRun, ParkedTaskNode, SCHEMA_VERSION, load_parked_run,
};
use super::super::retention::RetentionExpiresAt;
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
/// The sibling/replacement probe's registered name — ungated by
/// construction, so a driven task leaves run-level evidence without
/// touching the park machinery.
const PROBE_TOOL: &str = "deploy_probe";
/// The never-started sibling's final turn text — the wire marker the
/// coordinator-loop frame keys its final-answer-presence scan on.
const SIBLING_DONE: &str = "deployed and settled";
/// The failed resumed worker's final report — the marker the re-plan
/// frame keys its disjunctive scan on.
const FAILED_TEXT: &str = "the apply failed: the cluster rejected the manifest";
/// The replacement task's final turn text.
const REPLACEMENT_DONE: &str = "recovered and settled";
/// The mixed wave's completing sibling's marker — the text riding its
/// submit_result turn, the wave frames' order pin for that sibling.
const WAVE_SIBLING_DONE: &str = "checks done and settled";
/// The follow-up wave sibling's marker — the same shape, second wave.
const FOLLOWUP_DONE: &str = "the follow-up verified the rollout";
/// The restored-failure fixture's already-failed node — the task the
/// checkpoint recorded as Failed before the park, its description the
/// seeded failure history carries.
const RESTORED_FAILURE_DESC: &str = "Watch the rollout";
/// The restored failure's error text — the literal the history-line pins
/// key on.
const RESTORED_FAILURE_ERROR: &str = "the rollout stalled: the pod never went ready";
/// The goal-distinct fixture's raw query — the same literal every
/// standard fixture's document carries.
const CHECKPOINT_QUERY: &str = "Deploy the service";
/// The goal-distinct fixture's stored plan goal — deliberately a
/// different string than the query, so a goal=query reconstruction
/// cannot pass as a restoration.
const CHECKPOINT_GOAL: &str = "Ship the payments service to production with zero downtime";
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
    /// The default world's 207 receiver, held so it stays alive for the
    /// World's lifetime. `world_over_hitl` starts every world with an idle
    /// placeholder; the default `world()` swaps in the real receiver after
    /// construction.
    _receiver: tokio::task::JoinHandle<()>,
}

/// The persistent scripted receiver the default world parks against: one
/// ephemeral listener serving up to 64 sequential POST connections, each
/// answered with an empty `207 Multi-Status` (the shape that registers the
/// gated call for GET polling), then the loop exits. No request capture —
/// the frames seed and read the registry directly. The std listener binds
/// synchronously and converts inside the spawned task, so the caller needs
/// no await.
fn park_receiver() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("receiver listener binds");
    let url = format!(
        "http://{}",
        listener.local_addr().expect("receiver address")
    );
    listener
        .set_nonblocking(true)
        .expect("receiver listener goes non-blocking for tokio");
    let handle = tokio::spawn(async move {
        let listener =
            tokio::net::TcpListener::from_std(listener).expect("async receiver listener");
        for _ in 0..64 {
            let (mut socket, _) = listener.accept().await.expect("receiver accepts");
            let _ = crate::hitl::read_full_request(&mut socket).await;
            let response = "HTTP/1.1 207 Multi-Status\r\ncontent-type: application/json\r\n\
                            content-length: 0\r\nconnection: close\r\n\r\n";
            socket.write_all(response.as_bytes()).await.ok();
            socket.shutdown().await.ok();
        }
    });
    (url, handle)
}

/// The default world: parks through the 207 bridge on the poll-delivery
/// route — the one route runtime admission parks; the conversational
/// channel never durable-parks.
fn world() -> World {
    let (url, receiver) = park_receiver();
    let mut world = world_over_hitl(|registry| {
        let config = aura_config::HitlConfig {
            require_approval: vec![aura_config::GlobPattern::new("kubectl_*").unwrap()],
            park: aura_config::ParkConfig {
                enabled: true,
                bind_identity: false,
                park_ttl: aura_config::ParkTtl::default(),
            },
            route: aura_config::DecisionRouteConfig::Webhook {
                url: aura_config::WebhookUrl::new(&url).unwrap(),
                timeout_secs: 3600,
                headers: HashMap::new(),
                headers_from_request: HashMap::new(),
                tool_headers_from_response: aura_config::ToolHeaderMappings::default(),
                delivery: aura_config::WebhookDelivery::Poll,
                poll_url: None,
                poll_interval_secs: 10,
                poll_request_timeout_secs: 30,
                receiver_wait_timeout_secs: 900,
            },
        };
        crate::hitl::HitlRuntime::from_config(&config, registry, None, None)
    });
    world._receiver = receiver;
    world
}

/// Build a world over the caller's HITL runtime: the default world parks
/// through the 207 bridge on the poll-delivery route; the identity frames
/// swap in a poll-delivery webhook whose `tool_headers_from_response`
/// mapping arms the reify-side identity rule.
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
        // Idle placeholder so the construction stays total: the default
        // `world()` swaps in its live 207 receiver after construction, and
        // the identity frames never connect to a receiver at all.
        _receiver: tokio::spawn(async {}),
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
                park_ttl: aura_config::ParkTtl::default(),
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
                receiver_wait_timeout_secs: 900,
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
        authority: crate::hitl::ApprovalAuthority::WebhookPoll,
        egress_headers: None,
        acknowledgment: crate::hitl::AcknowledgmentState::RequiresNotification,
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
        .resolve(
            &decision(),
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ApprovalDecision::Approved.into(),
        )
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
        .resolve(
            &decision_b(),
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ApprovalDecision::Approved.into(),
        )
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
            crate::hitl::ApprovalAuthority::WebhookPoll,
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
            .resolve(
                &id,
                crate::hitl::ApprovalAuthority::WebhookPoll,
                ApprovalDecision::Approved.into(),
            )
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
        .resolve(
            &decision(),
            crate::hitl::ApprovalAuthority::WebhookPoll,
            first.into(),
        )
        .await
        .expect("record the pivot pair's first decision");
    world
        .registry
        .resolve(
            &decision_pivot_2(),
            crate::hitl::ApprovalAuthority::WebhookPoll,
            second.into(),
        )
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
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ResolvedDecision::approved(Some(crate::approver_headers::tests::captured_overrides(
                "x-forwarded-user",
                "tok",
            ))),
        )
        .await
        .expect("record the first duplicate's approval with identity");
    world
        .registry
        .resolve(
            &decision_2(),
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ApprovalDecision::Approved.into(),
        )
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

/// The tool-result text the substitution maps for `FailingTool`'s
/// ordinary execution `Err`: the `ToolServerError`'s raw `to_string`,
/// un-JSON-quoted — the Err path renders raw where the Ok path quotes,
/// and the fixed prefix stack (`Toolset error` wrapping the toolset's
/// and the boxed `ToolCallError`s) is the byte-stable rendering the
/// parity pin keys on.
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
/// reconstruction removes from the rebuilt context before the worker
/// ever streams it.
fn sentinel_prompt() -> rig::completion::Message {
    sentinel_prompt_for(CALL_ID)
}

/// The checkpointed prompt carrying the sentinel for the given pending call
/// id — one placeholder slot per parked call, the slot the rebuild
/// replaces in place.
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
/// prompt carries the placeholder keyed by the pending call id — the
/// tool-result user message the segment preflight's witness requires.
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

/// The sibling-carrying checkpoint: the standard one-awaiting-node
/// fixture plus one never-started Pending sibling (task 5) on the same
/// worker — stored bare, exactly the shape `build_document` writes for a
/// Pending node (no attempt, history, prompt, or pending calls). The
/// fixture the coordinator-loop frame drives.
fn sibling_pending_document(world: &World) -> ParkedRun {
    let mut document = sentinel_document(world);
    document.plan.tasks.push(ParkedTaskNode {
        task_id: 5,
        description: "Run the post-apply deployment checks".to_string(),
        dependencies: vec![],
        worker: Some("operations".to_string()),
        rationale: String::new(),
        status: TaskStatus::Pending,
        result: None,
        error: None,
        failure_category: None,
        attempt: None,
        history: None,
        current_prompt: None,
        pending: None,
    });
    document
}

/// One bare Pending sibling node — exactly the shape `build_document`
/// writes for a never-started task: no attempt, history, prompt, or
/// pending calls.
fn pending_sibling_node(
    task_id: usize,
    description: &str,
    dependencies: Vec<usize>,
) -> ParkedTaskNode {
    ParkedTaskNode {
        task_id,
        description: description.to_string(),
        dependencies,
        worker: Some("operations".to_string()),
        rationale: String::new(),
        status: TaskStatus::Pending,
        result: None,
        error: None,
        failure_category: None,
        attempt: None,
        history: None,
        current_prompt: None,
        pending: None,
    }
}

/// The parking-sibling checkpoint: the standard one-awaiting-node fixture
/// plus one never-started Pending sibling (task 5) — the fixture the
/// loop-re-park pair-retention frame drives, the sibling's scripted
/// worker issuing a gated call that re-parks the resumed run.
fn sibling_parks_document(world: &World) -> ParkedRun {
    let mut document = sentinel_document(world);
    document.plan.tasks.push(pending_sibling_node(
        5,
        "Run the post-apply deployment checks",
        vec![],
    ));
    document
}

/// The mixed-wave checkpoint: the standard fixture plus a completing
/// sibling (task 1) and a parking sibling (task 0) — plan order
/// deliberately REVERSED against the task-id merge, so the wave frames
/// pin the merge's sort, not the workers' build order.
fn sibling_wave_document(world: &World) -> ParkedRun {
    let mut document = sentinel_document(world);
    document
        .plan
        .tasks
        .push(pending_sibling_node(1, "Run the deploy checks", vec![]));
    document
        .plan
        .tasks
        .push(pending_sibling_node(0, "Watch the gated rollout", vec![]));
    document
}

/// The follow-up-wave checkpoint: the mixed-wave fixture plus one more
/// sibling (task 2) dependent on the completing sibling (task 1) — the
/// second wave a later runnable task forms after the mixed first wave.
fn sibling_followup_wave_document(world: &World) -> ParkedRun {
    let mut document = sibling_wave_document(world);
    document.plan.tasks.push(pending_sibling_node(
        2,
        "Verify the rollout landed",
        vec![1],
    ));
    document
}

/// The goal-distinct checkpoint: query and `plan.goal` deliberately
/// different strings, everything else the standard sentinel fixture —
/// the fixture the goal-restoration frame drives.
fn distinct_goal_document(world: &World) -> ParkedRun {
    let mut document = sentinel_document(world);
    document.query = CHECKPOINT_QUERY.to_string();
    document.plan.goal = CHECKPOINT_GOAL.to_string();
    document
}

/// The restored-failure checkpoint: the standard one-awaiting-node fixture
/// plus one node that already FAILED before the park (task 6), with the
/// checkpoint's failure history carrying that failure under the parked
/// iteration — the fixture the failure-history frame drives.
fn restored_failure_document(world: &World) -> ParkedRun {
    let mut document = sentinel_document(world);
    document.failure_history = vec![FailedTaskRecord {
        description: RESTORED_FAILURE_DESC.to_string(),
        error: RESTORED_FAILURE_ERROR.to_string(),
        iteration: 1,
        worker: Some("operations".to_string()),
        category: FailureCategory::AgentTimeout,
    }];
    document.plan.tasks.push(ParkedTaskNode {
        task_id: 6,
        description: RESTORED_FAILURE_DESC.to_string(),
        dependencies: vec![],
        worker: Some("operations".to_string()),
        rationale: String::new(),
        status: TaskStatus::Failed,
        result: None,
        error: Some(RESTORED_FAILURE_ERROR.to_string()),
        failure_category: Some(FailureCategory::AgentTimeout),
        attempt: None,
        history: None,
        current_prompt: None,
        pending: None,
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
        retention_expires_at: RetentionExpiresAt::from_datetime(
            chrono::DateTime::parse_from_rfc3339(expires_at)
                .expect("golden fixture stamp parses")
                .with_timezone(&chrono::Utc),
        ),
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
    publish(document, &dir, RUN, None)
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

/// A completing worker's natural submit_result turn: the marker text
/// streams ahead of the tool call in the same turn (the live
/// submit_result decision short-circuit ends the stream one item past
/// the tool result, so a trailing text turn is never requested).
fn submit_result_turn(marker: &str, call: &str, summary: &str, result: &str) -> Value {
    json!({
        "role": "assistant",
        "id": null,
        "content": [
            { "text": marker },
            {
                "id": call,
                "call_id": null,
                "function": {
                    "name": "submit_result",
                    "arguments": {
                        "confidence": "high",
                        "result": result,
                        "summary": summary,
                    },
                },
                "signature": null,
                "additional_params": null,
            },
        ],
    })
}

/// The gated assistant turn a parking worker's snapshot carries: the
/// freshly issued call at full wire fidelity — the rig id and the
/// provider call id both keyed by the scripted call, exactly the drive
/// loop's re-park pin's gated-turn shape.
fn fresh_gated_turn() -> Value {
    json!({
        "role": "assistant",
        "id": null,
        "content": [
            {
                "id": FRESH_CALL_ID,
                "call_id": NEW_CALL_ID,
                "function": { "name": NEW_TOOL, "arguments": { "namespace": "stage" } },
                "signature": null,
                "additional_params": null,
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
/// executes through the substitution, the coordinator continuation
/// finishes the run naturally over a scripted respond_directly, and the
/// completed segment's turns are the run's NATURAL turns only — the
/// continuation's final assistant turn, then the coordinator's scripted
/// tail (R6: the outcome pair lives inside the rebuilt history the
/// continuation streamed from, not on the wire).
#[tokio::test]
async fn all_decided_grant_runs_the_segment_to_completion() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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
                    {
                        "role": "assistant",
                        "id": null,
                        "content": [{ "text": FINAL_TEXT }],
                    },
                    coordinator_tail_turn(),
                ]),
                "the completed segment carries the natural continuation turn and the \
                 coordinator's scripted tail, and nothing else — the R2 pair prepend \
                 is gone (R6: outcomes live in the rebuilt history)"
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
    let _drain = OverrideDrain;
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
    let _drain = OverrideDrain;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    // The decided tool's recording registration and the sentinel prompt are
    // the board-owner repair (logged on the card): without them the segment
    // would fault for fixture reasons — a prompt the preflight witness
    // refuses, a missing tool to invoke. The frame pins the run-id binding
    // only; execution assertions live elsewhere.
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
/// substitution, then node B's decided call executes exactly once with its
/// own arguments — and the coordinator continuation finishes the run (R6):
/// the completed turns are the two nodes' natural final turns plus the
/// scripted coordinator tail; the R2 pairs ride the RE-PARKED segment only.
/// Both scripts end without submit_result, so each node lands in the
/// loop's soft-failure shape; this frame pins executions, turns, and
/// cleanup, not node state. Completion removes the fresh and sibling
/// tickets together; the placeholder appears nowhere.
#[tokio::test]
async fn consumed_subset_re_park_preserves_the_sibling_and_completes_on_the_second_resume() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
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
        .resolve(
            &fresh_decision,
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ApprovalDecision::Approved.into(),
        )
        .await
        .expect("record the fresh approval");

    // Resume 2 drives both awaiting nodes in plan order: node A's build
    // first, then node B's — one override per build, in that order. The
    // continuation's coordinator finishes the run over a scripted
    // respond_directly (R6 natural finish).
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
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);

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
            { "role": "assistant", "id": null, "content": [{ "text": A_DONE }] },
            { "role": "assistant", "id": null, "content": [{ "text": B_DONE }] },
            coordinator_tail_turn(),
        ]),
        "the completed segment carries the two nodes' natural final turns in \
         segment order, then the coordinator's scripted tail — the R2 pair \
         prepends are gone (R6: outcomes live in the rebuilt histories)"
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
    let _drain = OverrideDrain;
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
        .resolve(
            &fresh_decision,
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ApprovalDecision::Approved.into(),
        )
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
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
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
            { "role": "assistant", "id": null, "content": [{ "text": FINAL_TEXT }] },
            coordinator_tail_turn(),
        ]),
        "the completed segment's turns are the natural continuation turn and the \
         scripted coordinator tail — the fresh call's pair rode the RE-PARKED \
         segment under its own id; the completed turns are natural only (R6)"
    );
    assert!(
        !serialized.to_string().contains(PARK_SENTINEL),
        "the placeholder appears nowhere in the serialized turns"
    );
}

/// A recorded approval executes exactly once through the worker's gated
/// pipeline, the real result reaches the model in the reconstructed
/// context (the outcome package the rebuilt history carries — the R2
/// wire pair rides the RE-PARKED segment only, R6), and the coordinator
/// continuation finishes the run over a scripted respond_directly: the
/// completed turns are the continuation's final turn plus the scripted
/// tail. The park placeholder appears nowhere on the wire.
#[tokio::test]
async fn approved_call_executes_once_and_rides_the_outcome_pair() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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
                    {
                        "role": "assistant",
                        "id": null,
                        "content": [{ "text": FINAL_TEXT }],
                    },
                    coordinator_tail_turn(),
                ]),
                "the completed segment's turns are the natural continuation turn and \
                 the scripted coordinator tail — the outcome pair lives inside the \
                 rebuilt history, not on the wire (R6)"
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
/// placeholder (the rebuilt history's outcome package — the R2 wire pair
/// rides the RE-PARKED segment only, R6), and the coordinator continuation
/// finishes the run over a scripted respond_directly — the completed turns
/// are the continuation's final turn plus the scripted tail; the worker
/// adapts; no result is fabricated. The context also carries the decided
/// call's assistant tool call — synthesized by the reconstruction (P45
/// stage 3: the fixture's history, like every single-call sentinel
/// fixture's, never captured it), the pairing providers require ahead of
/// every tool result.
#[tokio::test]
async fn denied_call_steers_without_executing_and_rides_the_denial_pair() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
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
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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
                    decided_call_turn(),
                    decided_result_turn(&tool_wire(&denial_text())),
                ]),
                "the worker's context carries the decided call's assistant tool call \
                 (synthesized by the reconstruction) ahead of the live denial text \
                 and its reason verbatim, in place of the placeholder"
            );

            let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
            assert_eq!(
                serialized,
                json!([
                    {
                        "role": "assistant",
                        "id": null,
                        "content": [{ "text": FINAL_TEXT }],
                    },
                    coordinator_tail_turn(),
                ]),
                "the completed segment's turns are the natural continuation turn and \
                 the scripted coordinator tail — the denial package lives inside the \
                 rebuilt history, not on the wire (R6)"
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
/// fails. The error text reaches the model in the reconstructed context,
/// pinned by exact equality: the rebuilt history's outcome package
/// carries the raw error rendering verbatim, so a refactor cannot
/// fabricate success text silently (the R2 wire pair rides the RE-PARKED
/// segment only, R6), and the coordinator continuation finishes the run
/// over a scripted respond_directly; no success is fabricated and no
/// placeholder survives.
#[tokio::test]
async fn tool_failure_becomes_result_text_and_the_segment_completes() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]);
    let requests = model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model,
        extra_tools: vec![Box::new(FailingTool::new(invocations.clone()))],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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
            decided_call_turn(),
            decided_result_turn(&tool_failure_wire()),
        ]),
        "the REBUILT worker history carries the raw error rendering verbatim — \
         the execution Err becomes the tool-result text the substitution maps, \
         never fabricated success"
    );
    let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
    assert_eq!(
        serialized,
        json!([
            {
                "role": "assistant",
                "id": null,
                "content": [{ "text": FINAL_TEXT }],
            },
            coordinator_tail_turn(),
        ]),
        "the completed segment's turns are the natural continuation turn and the \
         scripted coordinator tail — the error text rides the rebuilt history's \
         outcome package, not the wire (R6)"
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
    let _drain = OverrideDrain;
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
    let _drain = OverrideDrain;
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
/// denial steers with its normal outcome (Completed, live denial text in
/// the rebuilt history's outcome package, the wire pair itself riding the
/// RE-PARKED segment only per R6, zero invocations, the coordinator
/// continuation finishing the run over a scripted respond_directly),
/// never the identity fault. Stands alone from the approval fault above
/// so each fails at its own named point.
#[tokio::test]
async fn denied_without_identity_steers_normally_under_the_identity_route() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = identity_world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(
            RecordingTool::new(invocations.clone()).with_name(TOOL),
        )],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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
            {
                "role": "assistant",
                "id": null,
                "content": [{ "text": FINAL_TEXT }],
            },
            coordinator_tail_turn(),
        ]),
        "the denial steers to its normal outcome — the natural continuation turn \
         and the scripted coordinator tail, the denial package riding the rebuilt \
         history (R6)"
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
    let _drain = OverrideDrain;
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

/// FAULT — prompt-shape refusal at the segment preflight: a checkpointed
/// `current_prompt` with NO tool result at all — the pre-A1 bare-prompt
/// shape, `parked_document`'s `Message::user("tool results")`, a
/// checkpoint no park producer writes — is refused by the segment-wide
/// preflight, fatally, with the node-attributed `NotAToolResultPrompt`
/// diagnostic, BEFORE any tombstone or invocation across the whole
/// segment. This retires the fold's after-tombstone replace-miss fatal
/// (the reconstruction wiring removed the replace whose miss it pinned:
/// the preflight now refuses the malformed shape the old frame staged,
/// before the first tombstone instead of after the invocation); the
/// interrupted-state semantics for post-tombstone crashes stay pinned by
/// `executed_tombstones_refuse_with_the_interrupted_row`. The queued
/// worker override is still queued after the refusal — no worker build
/// consumed it, so no worker streamed.
#[tokio::test]
async fn tool_result_less_prompt_refuses_at_preflight_before_any_tombstone() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
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
        .expect_err("a tool-result-less prompt is refused at the segment preflight");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "awaiting node 0: the parked snapshot's current prompt is not the tool-result \
         message the park producers write",
        "the fault's diagnostic identifies the prompt-shape refusal, named to its node"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the refusal precedes every invocation across the segment"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "the refusal precedes the FIRST tombstone: no once-only evidence exists"
    );
    assert!(
        requests
            .lock()
            .expect("scripted-model request log")
            .is_empty(),
        "no worker streamed: the refusal precedes every worker build"
    );
    assert!(
        take_worker_override().is_some(),
        "no worker build consumed the queued override: the refusal precedes \
         every build"
    );
}

/// FAULT — the segment door (the reconstruction wiring's all-or-nothing
/// preflight, integration): a TWO-awaiting-node checkpoint where node A is
/// fully valid and node B's pending call carries an EMPTY call id — the
/// shape the gate's park arm records when the stream hook observed no
/// tool-call id — is refused at the segment-wide preflight with the
/// node-attributed `EmptyCallId` diagnostic naming node B, and NOTHING
/// runs: node A's valid input must not execute first (the per-node hazard
/// the segment door exists to close), so ZERO invocations and ZERO
/// tombstones across the whole segment, and no worker ever builds or
/// streams.
#[tokio::test]
async fn empty_call_id_on_node_b_refuses_the_whole_segment_before_any_tombstone() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let scale_invocations = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]);
    let requests = model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model,
        extra_tools: vec![
            Box::new(RecordingTool::new(apply_invocations.clone()).with_name(TOOL)),
            Box::new(RecordingTool::new(scale_invocations.clone()).with_name(TOOL_B)),
        ],
    }]);
    register_decided(&world).await;
    register_decided_b(&world).await;
    // The mixed checkpoint: node A (task 3) fully valid, node B (task 4)
    // carrying one pending call whose call id is empty — everything else
    // about both nodes is the shape a live park writes.
    let mut document = two_node_sentinel_document(&world);
    let node_b = document
        .plan
        .tasks
        .last_mut()
        .expect("the two-node fixture carries node B last");
    node_b
        .pending
        .as_mut()
        .expect("node B carries pending calls")[0]
        .call_id = String::new();
    publish_document(&world, &document).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided two-node run grants");
    let fault = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect_err("node B's empty call id refuses the whole segment");
    assert_eq!(
        continuation_diagnostic(&fault).as_ref(),
        "awaiting node 1: pending call member 0 carries an empty call id",
        "the refusal is node-attributed and names the faulting member"
    );
    assert!(
        apply_invocations
            .lock()
            .expect("apply invocation log")
            .is_empty()
            && scale_invocations
                .lock()
                .expect("scale invocation log")
                .is_empty(),
        "ZERO invocations across the segment: node A's valid input never executed"
    );
    let resuming = resuming_document(&world).await;
    assert!(
        resuming.executed.is_empty(),
        "ZERO tombstones across the segment: the refusal precedes the first one"
    );
    assert!(
        requests
            .lock()
            .expect("scripted-model request log")
            .is_empty(),
        "no worker streamed: the refusal precedes every worker build"
    );
    assert!(
        take_worker_override().is_some(),
        "no worker build consumed the queued override: the refusal precedes \
         every build"
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
    let _drain = OverrideDrain;
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
        .resolve(
            &fresh_decision,
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ApprovalDecision::Approved.into(),
        )
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
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);

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
            { "role": "assistant", "id": null, "content": [{ "text": FINAL_TEXT }] },
            coordinator_tail_turn(),
        ]),
        "the completed segment's turns are the natural continuation turn and the \
         scripted coordinator tail — resume 1's duplicate pairs rode the re-parked \
         segment under their own ids; the completed turns are natural only (R6)"
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
    let _drain = OverrideDrain;
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
    let _drain = OverrideDrain;
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
    let _drain = OverrideDrain;
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
    let _drain = OverrideDrain;
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
// spec; the P45 stage-3 wiring (the segment preflight, keyed resolution,
// and rebuild_context) flipped them green, unedited, retiring the fold's
// replace-miss fatal they were red on.
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
    let _drain = OverrideDrain;
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
    // The coordinator continuation finishes the run (R6): scripted
    // respond_directly, so the completing segment never reaches an
    // unscripted provider.
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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
    let _drain = OverrideDrain;
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
    // The coordinator continuation finishes the run (R6): scripted
    // respond_directly, so the completing segment never reaches an
    // unscripted provider.
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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
    let _drain = OverrideDrain;
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
    // The coordinator continuation finishes the run (R6): scripted
    // respond_directly, so the completing segment never reaches an
    // unscripted provider.
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
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

/// The re-park turn boundary after reconstruction (the stage-3 wiring's
/// boundary fix): the turns a re-parking segment reports for the re-parked
/// node start STRICTLY AFTER the history the continuation actually
/// streamed from — the rebuilt input, including the SYNTHESIZED turn the
/// reconstruction appended for the second (slotless) call, is never
/// re-emitted as segment turns — and the re-park commit's refreshed
/// blocking names the fresh decision. Modeled on
/// `re_park_mid_segment_carries_turns_and_the_new_blocking_entry`, over
/// the pivot fixture: the rebuilt history carries the synthesized second
/// call's turn beyond the checkpoint's recorded length, so a boundary
/// sliced at the CHECKPOINT's length would replay it here.
#[tokio::test]
async fn re_parked_turns_start_after_the_rebuilt_history_with_no_replayed_reconstruction() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let pivot_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
            ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                .with_call_id(NEW_CALL_ID),
        ])]),
        extra_tools: vec![
            Box::new(RecordingTool::new(pivot_invocations.clone()).with_name(TOOL)),
            Box::new(RecordingTool::new(pivot_invocations.clone()).with_name(TOOL_B)),
            Box::new(RecordingTool::new(fresh_invocations).with_name(NEW_TOOL)),
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
        .expect("the pivot segment re-parks on the fresh gated call");
    {
        let log = pivot_invocations.lock().expect("pivot invocation log");
        assert_eq!(
            log.len(),
            2,
            "both pivot calls execute exactly once through the substitution, in \
             document order"
        );
        assert_eq!(
            log[0].arguments,
            call_args(),
            "the first invocation is the first (sentinel-bearing) call"
        );
        assert_eq!(
            log[1].arguments,
            call_args_b(),
            "the second invocation is the second (slotless) call"
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
                decided_call_turn_for(PIVOT_CALL_ID_2, TOOL_B, &call_args_b()),
                decided_result_turn_for(PIVOT_CALL_ID_2, &echo_tool_result_wire()),
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
        "the re-parked turns are the two decided pairs and the gated turn ONLY: \
         the rebuilt input — the synthesized second call's turn included — is \
         not replayed as segment turns; the turns start strictly after the \
         rebuilt history"
    );
    let serialized = serde_json::to_value(turns.as_slice()).expect("turns serialize");
    assert!(
        !serialized.to_string().contains(PARK_SENTINEL),
        "the placeholder appears nowhere in the serialized turns"
    );
}

/// Each turn in its wire form, for marker scans over the segment turns.
fn serialized_turns(turns: &[rig::completion::Message]) -> Vec<String> {
    turns
        .iter()
        .map(|message| serde_json::to_string(message).expect("turn serializes"))
        .collect()
}

/// A frame's own panic must not leak its undriven worker overrides into
/// the next consumer's builds: the queues are take-once and
/// process-global, and a frame that fails mid-test — a regression, or a
/// staged red while its fill is pending — never reaches an end-of-test
/// drain. Drains on drop, unwind included; instantiate right after
/// installing. Drains the coordinator queue beside the worker queue — a
/// frame that never reached its continuation entry leaves the
/// coordinator override unconsumed too.
struct OverrideDrain;

impl Drop for OverrideDrain {
    fn drop(&mut self) {
        while take_worker_override().is_some() {
            // drained
        }
        while take_coordinator_override().is_some() {
            // drained
        }
    }
}

/// The scripted coordinator's deterministic final answer — the turn text
/// every completing frame's coordinator script carries, so the natural
/// tail rides the completed turns as a literal.
const COORD_FINAL_ANSWER: &str = "the resumed run is complete";

/// The scripted coordinator turn every completing frame installs: one
/// respond_directly decision whose turn text is the final answer — the
/// deterministic natural finish (R6), routed through the real routing
/// toolset exactly like a live coordinator.
fn coordinator_direct_turn() -> ScriptedTurn {
    ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
        "call_route",
        "respond_directly",
        json!({
            "response": COORD_FINAL_ANSWER,
            "routing_rationale": "the resumed run's tasks are done",
        }),
    )])
    .with_text(COORD_FINAL_ANSWER)
}

/// The coordinator tail turn every completed-segment pin embeds: the
/// scripted final answer as the natural last turn.
fn coordinator_tail_turn() -> Value {
    json!({
        "role": "assistant",
        "id": null,
        "content": [{ "text": COORD_FINAL_ANSWER }],
    })
}

/// Whether an assistant turn carrying non-empty text rides strictly
/// after the last turn containing `marker` — the presence pin for the
/// coordinator's natural final-answer turns (the R6 natural-finish
/// ruling): the later wire-level unit pins the envelope; here only
/// existence beyond the last worker turn is pinned. Panics when no turn
/// carries the marker: the marker is the fixture's own scripted text,
/// so its absence is a wire-shape break, not a negative answer.
fn coordinator_answered_after(turns: &[rig::completion::Message], marker: &str) -> bool {
    let serialized = serialized_turns(turns);
    let Some(at) = serialized.iter().rposition(|s| s.contains(marker)) else {
        panic!("the segment turns carry the turn marked `{marker}`: {serialized:?}");
    };
    turns.iter().skip(at + 1).any(|message| {
        matches!(
            message,
            rig::completion::Message::Assistant { content, .. }
                if content.iter().any(|item| matches!(
                    item,
                    rig::message::AssistantContent::Text(text)
                        if !text.text.trim().is_empty()
                ))
        )
    })
}

/// The rendered continuation prompt from a scripted coordinator request:
/// rig folds the call's prompt into the request's trailing user turn, so
/// the last user message's text is the decision context the coordinator
/// actually received.
fn continuation_prompt(request: &rig::completion::CompletionRequest) -> String {
    request
        .chat_history
        .iter()
        .filter_map(|message| match message {
            rig::completion::Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|item| match item {
                        rig::message::UserContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .last()
        .expect("the continuation request carries its prompt as a user turn")
}

// ====================================================================
// Stage 6 coordinator-loop frames (the R6 natural-finish ruling, plus
// the segment_plan goal restoration): a decided resume re-enters the
// coordinator iteration loop over the checkpoint's restored state —
// driving never-started siblings, re-planning past worker failures,
// restoring the stored plan goal, and carrying restored failures into
// the loop's history exactly once. Coordinators stay scripted through
// the dedicated override queue; the pins live at the loop level (probe
// invocations, decision-context prompts, re-published checkpoints,
// cleanup).
// ====================================================================

/// STAGE 6 (R6 natural-finish): a checkpoint with one awaiting node
/// (its single gated call decided approved, sentinel and registered
/// ticket staged the faithful producer way) plus one never-started
/// Pending sibling. The resume must resume the COORDINATOR ITERATION
/// LOOP — restore the coordinator conversation, routing, iteration,
/// and failure history from the checkpoint and continue through
/// plan_with_routing — not just re-enter the executor: after the
/// approved call executes exactly once through the substitution, the
/// never-started sibling RUNS, and the run completes with the
/// coordinator's natural final-answer turns beyond the last worker
/// turn. Completion still deletes the checkpoint and removes the
/// consumed decisions. Both workers' continuations call the real
/// `submit_result` (registered through `add_all_tools`), so both nodes
/// carry the loop's SUCCESS semantics.
#[tokio::test]
async fn coordinator_resumes_after_awaiting_nodes_and_drives_never_started_siblings_to_completion()
{
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let probe_invocations = Arc::new(Mutex::new(Vec::new()));
    // Per-resume install, in build order: the awaiting node's worker
    // first (the substitution machinery builds it), then the sibling's
    // (a build only the resumed coordinator loop can make). The drain
    // guard keeps a panicking frame from leaking the sibling's undriven
    // override into another consumer's builds.
    let _drain = OverrideDrain;
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_a",
                    "submit_result",
                    json!({
                        "summary": "apply done",
                        "result": "applied cleanly",
                        "confidence": "high",
                    }),
                )]),
                ScriptedTurn::text(A_DONE),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_probe_b",
                    PROBE_TOOL,
                    json!({ "environment": "prod" }),
                )]),
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_b",
                    "submit_result",
                    json!({
                        "summary": "deploy checks done",
                        "result": "deployed cleanly",
                        "confidence": "high",
                    }),
                )])
                // The marker rides the submit_result turn's own text:
                // the decision short-circuit ends the stream one item
                // past the submit_result tool result, so a trailing
                // text turn is never requested (board-owner repair
                // ruling).
                .with_text(SIBLING_DONE),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(probe_invocations.clone()).with_name(PROBE_TOOL),
            )],
        },
    ]);
    // The coordinator continuation (R6): scripted respond_directly, the
    // deterministic natural finish.
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
    register_decided(&world).await;
    publish_document(&world, &sibling_pending_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the resumed run drives to completion");
    let turns = match segment {
        SegmentResult::Completed { turns } => turns,
        other => panic!("expected a completed segment, got {other:?}"),
    };
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            1,
            "the approved call executes exactly once through the substitution"
        );
        assert_eq!(
            apply_log[0].arguments,
            call_args(),
            "the single invocation carries the recorded call's arguments"
        );
    }
    {
        let probe_log = probe_invocations
            .lock()
            .expect("sibling probe invocation log");
        assert_eq!(
            probe_log.len(),
            1,
            "the never-started sibling RUNS under the resumed coordinator loop; zero \
             invocations recorded: the segment ended after the awaiting node and no \
             coordinator turn occurred"
        );
        assert_eq!(
            probe_log[0].arguments,
            json!({ "environment": "prod" }),
            "the sibling's invocation carries its scripted arguments"
        );
    }
    assert!(
        coordinator_answered_after(turns.as_slice(), SIBLING_DONE),
        "an assistant final-answer turn follows the sibling's last turn — the \
         coordinator finishes its turn naturally after the workers (R6): {:?}",
        serialized_turns(turns.as_slice())
    );
    assert!(
        !parked_document_path(&world).exists() && !resuming_document_path(&world).exists(),
        "completion deletes the checkpoint under both its names"
    );
    assert!(
        world
            .registry
            .try_parked(&decision())
            .await
            .expect("the store reads")
            .is_none(),
        "completion removes the consumed decision from the store"
    );
}

/// STAGE 6 (R6 natural-finish): an awaiting node whose resumed worker
/// FAILS — the continuation streams a failure report without calling
/// `submit_result`, the SoftFailure shape the normal execution loop
/// already defines (the `structured_output.is_none()` branch). The
/// resumed node maps through that same rule, so the coordinator loop
/// actually SEES the failure: the continuation request the coordinator
/// receives carries the node as a FAILED task in the plan state and a
/// failure-history entry under the RESUMED iteration. The loop then
/// continues past it — a replacement (or retried) task's worker runs or
/// a final answer lands; the choice is the coordinator's, the ruling
/// demands the continuation, not a particular one. The approved call
/// still executed exactly once — the failure is downstream of the
/// execution.
#[tokio::test]
async fn resumed_coordinator_replans_when_a_resumed_worker_fails() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let probe_invocations = Arc::new(Mutex::new(Vec::new()));
    // Per-resume install, in build order: the failing node's worker
    // first, then the replacement's (a build only a coordinator re-plan
    // can make), under the red-safe drain guard.
    let _drain = OverrideDrain;
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FAILED_TEXT)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_probe_r",
                    PROBE_TOOL,
                    json!({ "environment": "prod" }),
                )]),
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_r",
                    "submit_result",
                    json!({
                        "summary": "recovered",
                        "result": "recovered and applied",
                        "confidence": "high",
                    }),
                )]),
                ScriptedTurn::text(REPLACEMENT_DONE),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(probe_invocations.clone()).with_name(PROBE_TOOL),
            )],
        },
    ]);
    // The coordinator continuation (R6): scripted respond_directly — the
    // re-plan pin's final-answer leg. The model is cloned before the
    // install so the frame can read the request log the consumed
    // override recorded — the loop's decision context is the pin's
    // subject.
    let coordinator = ScriptedCompletionModel::new(vec![coordinator_direct_turn()]);
    let coordinator_requests = coordinator.requests();
    install_coordinator_overrides(vec![CoordinatorOverride { model: coordinator }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the resumed run completes after the worker failure");
    let turns = match segment {
        SegmentResult::Completed { turns } => turns,
        other => panic!("expected a completed segment, got {other:?}"),
    };
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            1,
            "the approved call executes exactly once — the failure is downstream of \
             the execution"
        );
    }
    assert!(
        serialized_turns(turns.as_slice())
            .iter()
            .any(|s| s.contains(FAILED_TEXT)),
        "the failed worker's report rode the segment turns"
    );
    // The load-bearing re-plan pin: the failure reached the loop's
    // decision context. The continuation request the coordinator
    // received carries the failed node as plan state and records the
    // failure under the resumed iteration (the checkpoint parked at
    // iteration 1; the resumed loop executes iteration 2).
    let recorded = coordinator_requests
        .lock()
        .expect("coordinator request log")
        .clone();
    assert_eq!(
        recorded.len(),
        1,
        "the resumed loop makes exactly one continuation call"
    );
    let prompt = continuation_prompt(&recorded[0]);
    assert!(
        prompt.contains(&format!(
            "- Task 3: Gated apply → failed [soft_failure]: {FAILED_TEXT}"
        )),
        "the loop's decision context carries the failed node as plan state: {prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "- Iteration 2: \"Gated apply\" (worker: operations) — [soft_failure] {FAILED_TEXT}"
        )),
        "the failure history records the failure under the RESUMED iteration: {prompt}"
    );
    // The loop-continuation leg: the coordinator actually continued past
    // the failure — a replacement build or a final answer, whichever it
    // chose.
    let probe_count = probe_invocations
        .lock()
        .expect("replacement probe invocation log")
        .len();
    let answered = coordinator_answered_after(turns.as_slice(), FAILED_TEXT);
    assert!(
        probe_count == 1 || answered,
        "the resumed coordinator continues past the failure: a replacement (or \
         retried) task's worker ran (probe invocations: {probe_count}) OR a \
         final-answer turn follows the failure report ({answered}); neither \
         happened — the segment ended with no coordinator turn"
    );
    assert!(
        !parked_document_path(&world).exists() && !resuming_document_path(&world).exists(),
        "completion deletes the checkpoint under both its names"
    );
    assert!(
        world
            .registry
            .try_parked(&decision())
            .await
            .expect("the store reads")
            .is_none(),
        "completion removes the consumed decision from the store"
    );
}

/// STAGE 6: a checkpoint with one pre-existing Failed node plus one
/// awaiting node resumes, the awaiting node completes, and the failure
/// history the coordinator's continuation request carries holds the
/// restored failure EXACTLY ONCE — under the parked iteration, never
/// re-recorded under the resumed one. The restored node stays visible
/// as failed plan state; what must not happen is the duplication that
/// paints the restored failure as a fresh one (and a repeated-failure
/// pattern the resumed run never observed).
#[tokio::test]
async fn restored_failures_are_not_re_recorded_under_the_resumed_iteration() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let _drain = OverrideDrain;
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![
            ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                "call_sub_a",
                "submit_result",
                json!({
                    "summary": "apply done",
                    "result": "applied cleanly",
                    "confidence": "high",
                }),
            )]),
            ScriptedTurn::text(A_DONE),
        ]),
        extra_tools: vec![Box::new(
            RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
        )],
    }]);
    let coordinator = ScriptedCompletionModel::new(vec![coordinator_direct_turn()]);
    let coordinator_requests = coordinator.requests();
    install_coordinator_overrides(vec![CoordinatorOverride { model: coordinator }]);
    register_decided(&world).await;
    publish_document(&world, &restored_failure_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the resumed run completes");
    assert!(
        matches!(segment, SegmentResult::Completed { .. }),
        "the segment completes: {segment:?}"
    );
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            1,
            "the awaiting node's decided call executes exactly once through the \
             substitution before its completion"
        );
    }
    let recorded = coordinator_requests
        .lock()
        .expect("coordinator request log")
        .clone();
    assert_eq!(
        recorded.len(),
        1,
        "the resumed loop makes exactly one continuation call"
    );
    let prompt = continuation_prompt(&recorded[0]);
    let restored_line = format!(
        "- Iteration 1: \"{RESTORED_FAILURE_DESC}\" (worker: operations) — [agent_timeout] \
         {RESTORED_FAILURE_ERROR}"
    );
    assert_eq!(
        prompt.matches(&restored_line).count(),
        1,
        "the restored failure rides the history exactly once, under the parked \
         iteration: {prompt}"
    );
    assert!(
        !prompt.contains(&format!("- Iteration 2: \"{RESTORED_FAILURE_DESC}\"")),
        "the restored failure is not re-recorded under the resumed iteration: {prompt}"
    );
    assert!(
        !prompt.contains("OBSERVED PATTERNS"),
        "no repeated-failure pattern renders for a failure the resumed run did not \
         observe: {prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "- Task 6: {RESTORED_FAILURE_DESC} → failed [agent_timeout]: {RESTORED_FAILURE_ERROR}"
        )),
        "the restored node stays visible to the decision context as failed plan \
         state: {prompt}"
    );
    assert!(
        prompt.contains("- Task 3: Gated apply (confidence: high)"),
        "the awaiting node completed through the loop's success semantics, structured \
         output and all: {prompt}"
    );
}

/// STAGE 6: `segment_plan` must restore the CHECKPOINT's `plan.goal`,
/// not rebuild the plan from the raw query. The fixture makes query and
/// goal deliberately different strings; the honest observable is the
/// re-published checkpoint's own `plan.goal` — the run-level projection
/// of the segment plan's goal through the re-park commit
/// (`build_document` stamps `plan.goal`), so a goal=query
/// reconstruction writes the query back into the checkpoint.
#[tokio::test]
async fn segment_plan_restores_the_checkpoint_goal_not_the_query() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let world = world();
    let _drain = OverrideDrain;
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
            ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                .with_call_id(NEW_CALL_ID),
        ])]),
        extra_tools: vec![
            Box::new(RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL)),
            Box::new(RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(NEW_TOOL)),
        ],
    }]);
    register_decided(&world).await;
    publish_document(&world, &distinct_goal_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the continuation re-parks on the fresh gated call");
    assert!(
        matches!(segment, SegmentResult::Parked { .. }),
        "the goal fixture's continuation re-parks: {segment:?}"
    );
    let republished = load_parked_run(&parked_document_path(&world))
        .await
        .expect("the re-park re-published the checkpoint under the parked name");
    assert_eq!(
        republished.plan.goal, CHECKPOINT_GOAL,
        "the re-published checkpoint carries the CHECKPOINT's plan goal — the \
         segment plan must restore the stored goal, not rebuild from the raw query"
    );
    assert_eq!(
        republished.query, CHECKPOINT_QUERY,
        "the query field stays the raw query — goal and query are distinct fields"
    );
}

// ====================================================================
// Phase A frontier review round 1 (findings 1-3): the pair-retention,
// failure-history, and wave-ordering repairs on the PARKED arm. The
// frames pin the re-parked segment's whole turns array (every decided
// pair consumed in the segment rides it, each node's pairs ahead of
// that node's own turns; parked wave turns merge by task id) and the
// re-published checkpoint's failure history (the original plus the
// drive's newly observed failures). The completed arm's pair-free shape
// stays pinned by the frames above.
// ====================================================================

/// A completed node's decided pairs survive a sibling's early re-park
/// (finding 2): node A completes through its decided call and node B's
/// continuation re-parks the segment in the drive loop, and the parked
/// turns carry BOTH nodes' pairs — A's ahead of A's own continuation
/// turns (document order), B's ahead of B's gated turn — with the fresh
/// blocking entry naming B's newly gated call.
#[tokio::test]
async fn a_completed_nodes_pairs_ride_the_early_re_park() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let scale_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    // Build order: node A first, then node B.
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_a",
                    "submit_result",
                    json!({
                        "summary": "apply done",
                        "result": "applied cleanly",
                        "confidence": "high",
                    }),
                )])
                .with_text(A_DONE),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![
                Box::new(RecordingTool::new(scale_invocations.clone()).with_name(TOOL_B)),
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
        .expect("node B's continuation re-parks the segment");
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            1,
            "node A's decided call executes exactly once"
        );
        assert_eq!(apply_log[0].arguments, call_args());
    }
    {
        let scale_log = scale_invocations.lock().expect("scale invocation log");
        assert_eq!(
            scale_log.len(),
            1,
            "node B's decided call executes exactly once"
        );
        assert_eq!(scale_log[0].arguments, call_args_b());
    }
    {
        let fresh_log = fresh_invocations.lock().expect("fresh invocation log");
        assert!(fresh_log.is_empty(), "the newly gated call never executes");
    }
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
                        submit_result_turn(A_DONE, "call_sub_a", "apply done", "applied cleanly"),
                        decided_call_turn_for(CALL_ID_B, TOOL_B, &call_args_b()),
                        decided_result_turn_for(CALL_ID_B, &echo_tool_result_wire()),
                        fresh_gated_turn(),
                    ],
                    "blocking": [
                        {
                            "decision_id": "<fresh decision id>",
                            "tool": NEW_TOOL,
                            "expires_at": "<fresh expiry>",
                        },
                    ],
                }),
                "the parked turns carry BOTH nodes' pairs — A's ahead of A's \
                 continuation turns, B's ahead of B's gated turn — and the \
                 fresh blocking entry names B's newly gated call"
            );
        }
        other => panic!("expected a re-parked segment, got {other:?}"),
    }
}

/// An early re-park publishes the drive loop's newly observed failures
/// (finding 1): node A soft-fails its continuation (no submit_result)
/// and node B re-parks, and the re-published checkpoint's failure
/// history carries the checkpoint's own history PLUS A's failure, stamped
/// with the iteration the restored plan executes under — the same
/// derivation the completion path's collector applies. Resume 2 then
/// drives B to completion, and A's failure-history entry appears EXACTLY
/// ONCE in the coordinator's decision context, under the resumed
/// iteration: never dropped, never re-recorded.
#[tokio::test]
async fn an_early_re_park_publishes_the_drive_loops_new_failures() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let scale_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    // Resume 1, build order: node A (soft-fails its continuation), then
    // node B (its continuation's new gated call re-parks the segment).
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FAILED_TEXT)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![
                Box::new(RecordingTool::new(scale_invocations.clone()).with_name(TOOL_B)),
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
        .expect("node B's continuation re-parks the segment");
    let blocking = match segment {
        SegmentResult::Parked { blocking, .. } => blocking,
        other => panic!("expected a re-parked segment, got {other:?}"),
    };
    let fresh: Vec<_> = blocking
        .as_slice()
        .iter()
        .filter(|entry| entry.tool.as_ref() == NEW_TOOL)
        .collect();
    assert_eq!(
        fresh.len(),
        1,
        "exactly one fresh blocking entry: {blocking:?}"
    );
    let fresh_decision = fresh[0].decision_id;

    // The published history: the checkpoint's own (empty) plus A's
    // soft failure, under the resumed iteration — the iteration the
    // restored plan executes under, matching the completion path's
    // stamp for the same transition.
    let republished = load_parked_run(&parked_document_path(&world))
        .await
        .expect("the re-park re-published the checkpoint under the parked name");
    assert_eq!(
        republished.failure_history.len(),
        1,
        "the published history carries exactly the drive's one new failure"
    );
    let record = &republished.failure_history[0];
    assert_eq!(record.description, "Gated apply");
    assert_eq!(record.error, FAILED_TEXT);
    assert_eq!(record.iteration, 2);
    assert_eq!(record.worker.as_deref(), Some("operations"));
    assert_eq!(record.category, FailureCategory::SoftFailure);
    let node_a = republished
        .plan
        .tasks
        .iter()
        .find(|node| node.task_id == 3)
        .expect("the published plan carries node A");
    assert!(
        node_a.status == TaskStatus::Failed,
        "node A landed in the loop's soft-failure shape: {:?}",
        node_a.status
    );
    assert_eq!(node_a.error.as_deref(), Some(FAILED_TEXT));
    assert_eq!(node_a.failure_category, Some(FailureCategory::SoftFailure));

    // Resume 2: approve the fresh call, drive node B to completion, and
    // read the coordinator's decision context — A's entry rides exactly
    // once, under the resumed iteration.
    world
        .registry
        .resolve(
            &fresh_decision,
            crate::hitl::ApprovalAuthority::WebhookPoll,
            ApprovalDecision::Approved.into(),
        )
        .await
        .expect("record the fresh approval");
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![
            ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                "call_sub_b",
                "submit_result",
                json!({
                    "summary": "scale done",
                    "result": "scaled cleanly",
                    "confidence": "high",
                }),
            )])
            .with_text(B_DONE),
        ]),
        extra_tools: vec![Box::new(
            RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL),
        )],
    }]);
    let coordinator = ScriptedCompletionModel::new(vec![coordinator_direct_turn()]);
    let coordinator_requests = coordinator.requests();
    install_coordinator_overrides(vec![CoordinatorOverride { model: coordinator }]);

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the re-published checkpoint grants the second resume");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the second segment completes");
    assert!(
        matches!(segment, SegmentResult::Completed { .. }),
        "the second segment completes: {segment:?}"
    );
    {
        let fresh_log = fresh_invocations.lock().expect("fresh invocation log");
        assert_eq!(fresh_log.len(), 1, "the fresh call executes exactly once");
        assert_eq!(fresh_log[0].arguments, json!({ "namespace": "stage" }));
    }
    let recorded = coordinator_requests
        .lock()
        .expect("coordinator request log")
        .clone();
    assert_eq!(
        recorded.len(),
        1,
        "the resumed loop makes exactly one continuation call"
    );
    let prompt = continuation_prompt(&recorded[0]);
    let resumed_line = format!(
        "- Iteration 2: \"Gated apply\" (worker: operations) — [soft_failure] {FAILED_TEXT}"
    );
    assert_eq!(
        prompt.matches(&resumed_line).count(),
        1,
        "A's failure-history entry appears EXACTLY once, under the resumed \
         iteration: {prompt}"
    );
    assert!(
        !prompt.contains("- Iteration 1: \"Gated apply\""),
        "A's failure is never recorded under the parked iteration: {prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "- Task 3: Gated apply → failed [soft_failure]: {FAILED_TEXT}"
        )),
        "node A stays visible to the decision context as failed plan state: {prompt}"
    );
    assert!(
        prompt.contains("- Task 4: Gated scale (confidence: high)"),
        "node B completed through the loop's success semantics: {prompt}"
    );
}

/// A completed awaiting node's decided pairs ride a LOOP re-park too
/// (finding 2): node A completes through its decided call, the
/// continuation drives a never-started sibling whose gated call
/// re-parks the resumed run, and the parked turns carry A's pair ahead
/// of A's own continuation turns, then the sibling's gated turn.
#[tokio::test]
async fn a_completed_nodes_pairs_ride_the_loop_re_park() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    // Build order: node A first (the drive loop), then the sibling's (a
    // build only the resumed coordinator loop can make).
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_a",
                    "submit_result",
                    json!({
                        "summary": "apply done",
                        "result": "applied cleanly",
                        "confidence": "high",
                    }),
                )])
                .with_text(A_DONE),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![Box::new(
                RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL),
            )],
        },
    ]);
    // The continuation builds its coordinator before the loop; the park
    // path skips the coordinator call, so the script is never consumed
    // by a request.
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
    register_decided(&world).await;
    publish_document(&world, &sibling_parks_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the sibling's gated call re-parks the resumed run");
    {
        let apply_log = apply_invocations.lock().expect("apply invocation log");
        assert_eq!(
            apply_log.len(),
            1,
            "node A's decided call executes exactly once"
        );
    }
    {
        let fresh_log = fresh_invocations.lock().expect("fresh invocation log");
        assert!(
            fresh_log.is_empty(),
            "the sibling's gated call never executes"
        );
    }
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
                        submit_result_turn(A_DONE, "call_sub_a", "apply done", "applied cleanly"),
                        fresh_gated_turn(),
                    ],
                    "blocking": [
                        {
                            "decision_id": "<fresh decision id>",
                            "tool": NEW_TOOL,
                            "expires_at": "<fresh expiry>",
                        },
                    ],
                }),
                "the parked turns carry A's pair ahead of A's continuation \
                 turns, then the parking sibling's gated turn"
            );
        }
        other => panic!("expected a re-parked segment, got {other:?}"),
    }
}

/// A parked sibling's snapshot turns join their ORIGINATING wave's
/// task-id merge (finding 3): in a wave where task 0 parks and task 1
/// completes, task 0's gated turn precedes task 1's completion turn —
/// the parked turns keep their wave position instead of trailing every
/// completed turn of every wave. The sibling nodes sit in the plan in
/// REVERSE id order, so the pin exercises the merge's task-id sort, not
/// the workers' build order. Node A's pair and turns ride ahead of the
/// wave (A completed in the drive loop).
#[tokio::test]
async fn parked_wave_turns_merge_into_their_wave_by_task_id() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    // Build order: node A (drive loop), then the wave's tasks in plan
    // order — the completing sibling (task 1) ahead of the parking one
    // (task 0), reverse of the id merge the frame pins.
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_a",
                    "submit_result",
                    json!({
                        "summary": "apply done",
                        "result": "applied cleanly",
                        "confidence": "high",
                    }),
                )])
                .with_text(A_DONE),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_w",
                    "submit_result",
                    json!({
                        "summary": "checks done",
                        "result": "checks passed",
                        "confidence": "high",
                    }),
                )])
                .with_text(WAVE_SIBLING_DONE),
            ]),
            extra_tools: vec![],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![Box::new(
                RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL),
            )],
        },
    ]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
    register_decided(&world).await;
    publish_document(&world, &sibling_wave_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the parking sibling re-parks the resumed run");
    {
        let fresh_log = fresh_invocations.lock().expect("fresh invocation log");
        assert!(
            fresh_log.is_empty(),
            "the parked sibling's gated call never executes"
        );
    }
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
                        submit_result_turn(A_DONE, "call_sub_a", "apply done", "applied cleanly"),
                        fresh_gated_turn(),
                        submit_result_turn(
                            WAVE_SIBLING_DONE,
                            "call_sub_w",
                            "checks done",
                            "checks passed",
                        ),
                    ],
                    "blocking": [
                        {
                            "decision_id": "<fresh decision id>",
                            "tool": NEW_TOOL,
                            "expires_at": "<fresh expiry>",
                        },
                    ],
                }),
                "the parked sibling's gated turn precedes the higher-id \
                 completing sibling's turn — the parked turns joined their \
                 wave's task-id merge, and node A's pair rides ahead of the \
                 wave"
            );
        }
        other => panic!("expected a re-parked segment, got {other:?}"),
    }
}

/// A later runnable wave stays AFTER the earlier wave's parked turns
/// (finding 3): task 2, dependent on the completing task 1, forms the
/// second wave once task 1 lands, and its turn rides after the whole
/// mixed first wave — the parked task 0's turns included.
#[tokio::test]
async fn a_followup_wave_stays_after_the_earlier_waves_parked_turns() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_a",
                    "submit_result",
                    json!({
                        "summary": "apply done",
                        "result": "applied cleanly",
                        "confidence": "high",
                    }),
                )])
                .with_text(A_DONE),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(apply_invocations.clone()).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_w",
                    "submit_result",
                    json!({
                        "summary": "checks done",
                        "result": "checks passed",
                        "confidence": "high",
                    }),
                )])
                .with_text(WAVE_SIBLING_DONE),
            ]),
            extra_tools: vec![],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                ScriptedToolCall::new(FRESH_CALL_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                    .with_call_id(NEW_CALL_ID),
            ])]),
            extra_tools: vec![Box::new(
                RecordingTool::new(fresh_invocations.clone()).with_name(NEW_TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_f",
                    "submit_result",
                    json!({
                        "summary": "follow-up done",
                        "result": "follow-up verified",
                        "confidence": "high",
                    }),
                )])
                .with_text(FOLLOWUP_DONE),
            ]),
            extra_tools: vec![],
        },
    ]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
    register_decided(&world).await;
    publish_document(&world, &sibling_followup_wave_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment(grant, &world.config, &HashMap::new())
        .await
        .expect("the parking sibling re-parks the resumed run after the follow-up wave");
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
                        submit_result_turn(A_DONE, "call_sub_a", "apply done", "applied cleanly"),
                        fresh_gated_turn(),
                        submit_result_turn(
                            WAVE_SIBLING_DONE,
                            "call_sub_w",
                            "checks done",
                            "checks passed",
                        ),
                        submit_result_turn(
                            FOLLOWUP_DONE,
                            "call_sub_f",
                            "follow-up done",
                            "follow-up verified",
                        ),
                    ],
                    "blocking": [
                        {
                            "decision_id": "<fresh decision id>",
                            "tool": NEW_TOOL,
                            "expires_at": "<fresh expiry>",
                        },
                    ],
                }),
                "the follow-up wave's turn rides after the whole mixed first \
                 wave — the parked task 0's gated turn included, not after \
                 it in append order"
            );
        }
        other => panic!("expected a re-parked segment, got {other:?}"),
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
