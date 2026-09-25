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
    CoordinatorOverride, ECHO_TOOL_RESULT, FreeformArgs, RecordingTool, ScriptedCompletionModel,
    ScriptedToolCall, ScriptedTurn, StallHook, WORKER_OVERRIDE_SERIAL, WorkerOverride,
    echo_tool_result_wire, install_coordinator_overrides, install_worker_overrides,
    take_coordinator_override, take_worker_override,
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

/// Drive one granted segment through the live borrowed-grant seam: the
/// retired atomic `run_segment` entry's test-side replacement. The segment
/// consumes the grant by reference and streams to a detached channel the
/// frame does not read — the frame's assertions are the segment's side
/// effects and its `ResumeStreamEnd`/fault terminal.
async fn run_segment_live(
    grant: ResumeGrant,
    config: &AgentRuntimeConfig,
    headers: &HashMap<String, String>,
) -> Result<ResumeStreamEnd, SegmentError> {
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(64);
    run_segment_borrowed(
        &grant,
        config,
        headers,
        event_tx,
        crate::UsageState::new(),
        None,
    )
    .await
}

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
/// The fixed per-call window the shared `node_approval` fixture stamps on a
/// parked ticket: distinct from `FUTURE_STAMP` so a blocking entry carrying
/// the document's retention stamp cannot pass as the call's own deadline.
const TICKET_STAMP: &str = "2098-01-01T00:00:00Z";
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
        // The ticket's OWN window, fixed and distinct from the document
        // retention stamp: the consult's blocking entries must carry this
        // per-call deadline, never the run-wide stamp.
        expires_at: chrono::DateTime::parse_from_rfc3339(TICKET_STAMP)
            .expect("the ticket stamp parses")
            .with_timezone(&chrono::Utc),
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
            "blocking": [entry(decision(), TOOL, TICKET_STAMP)],
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

/// Two POSTs racing the recovery of the same crashed empty resuming
/// document under the ordered entry: the reservation is step 2, so exactly
/// one evaluation holds the run before any rename begins — the winner
/// restores the parked name and answers the parked row, the loser answers
/// the running row without ever reaching the checkpoint or the store. No
/// fault on either side.
#[tokio::test]
async fn concurrent_posts_on_an_empty_resuming_run_answer_parked_and_running() {
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
    // Order-independent: exactly one parked body and one running body.
    let mut bodies = Vec::new();
    for outcome in [first, second] {
        let refusal = outcome.expect_err("the crashed run refuses");
        match refusal {
            ResumeRefusal::Conflict(row) => {
                bodies.push(serde_json::to_value(row).expect("the conflict row serializes"))
            }
            other => panic!("both answers are conflict rows, got {other:?}"),
        }
    }
    bodies.sort_by_key(|body| {
        body["code"]
            .as_str()
            .expect("the conflict row carries its code")
            .to_string()
    });
    assert_eq!(
        bodies,
        vec![
            json!({
                "code": "parked",
                "detail": "calls still await a decision",
                "blocking": [entry(decision(), TOOL, TICKET_STAMP)],
            }),
            json!({
                "code": "running",
                "detail": "another resume holds this run",
                "blocking": [],
            }),
        ],
        "one winner answers parked, one loser answers running"
    );
    assert!(
        parked_document_path(&world).exists(),
        "the winner's fenced rename-back restored the parked name"
    );
    assert!(
        !resuming_document_path(&world).exists(),
        "the resuming name is gone"
    );
}

/// The fenced rename-back's lease clone is the whole fence once its awaiter
/// is gone: with the rendezvous armed, the awaiting task is aborted mid-await
/// and every OUTER lease reference is dropped, and the run still reads live
/// until the released tail actually completes the rename — then, and only
/// then, the final reference drops and the run releases.
///
/// The conversion tail inside `convert_reserved` holds its lease clone
/// through the same binding shape; a rendezvous for it would need a
/// cross-module cfg(test) seam, so that proof is a recorded residual at S3
/// scope.
#[tokio::test]
async fn the_fenced_rename_tail_holds_the_reservation_after_its_awaiter_drops() {
    let world = Arc::new(world());
    stage_resuming_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;
    let run = ResumeRunId::parse(RUN).expect("the golden run id parses");
    let lease = world
        .claims
        .reserve(&run)
        .expect("the run reserves before the recovery rename");
    let guard = world.claims.arm_fenced_rename();

    let awaiter = {
        let world = Arc::clone(&world);
        let lease = lease.clone();
        tokio::spawn(async move {
            // The `'static` shape: the path and documents are built inside
            // the task, so nothing borrowed crosses the spawn boundary.
            let path = ValidatedResumePath::parse(SESSION, RUN).expect("the golden path validates");
            let docs = ResumeDocuments::for_path(&path, &world.memory_dir);
            world
                .claims
                .rename_back_under_reservation(&lease, &docs)
                .await
        })
    };

    // The tail signals arrival and holds before renaming; the blocking recv
    // runs off the async worker, and the guard comes back for the release.
    let guard = tokio::task::spawn_blocking(move || {
        guard
            .arrival
            .recv()
            .expect("the fenced tail signals arrival");
        guard
    })
    .await
    .expect("the arrival task completes");

    // The awaiting request dies mid-await and the test drops its own lease:
    // the blocking tail's clone is now the only holder.
    awaiter.abort();
    drop(lease);
    assert!(
        world.claims.is_live(&run),
        "the in-flight tail's lease clone keeps the run reserved after every outer reference dropped"
    );

    // The release lets the tail finish; the fence drops only then.
    guard.release.send(()).expect("the release channel is open");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while world.claims.is_live(&run) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !world.claims.is_live(&run),
        "the run releases once the tail actually completes"
    );
    assert!(
        parked_document_path(&world).exists(),
        "the released tail completed the rename-back"
    );
    assert!(
        !resuming_document_path(&world).exists(),
        "the resuming name is gone after the released tail"
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

/// The same missing ticket past the window is the expired row. A missing
/// ticket has no stored row, so no actual per-call deadline exists to
/// report: past retention the expired row is terminal and the teardown
/// sweep follows, and an empty blocking list is the honest shape (the
/// possibly-empty expired row is the F11 contract semantics) — synthesizing
/// the run-wide document stamp would restore exactly the projection this
/// cutover retires.
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
            "blocking": [],
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
            // The entry carries the ticket's OWN deadline, not the
            // document's retention stamp.
            "blocking": [entry(decision(), TOOL, TICKET_STAMP)],
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

/// How long the end-of-segment drain probe waits before declaring the
/// segment still blocked on its in-flight tail. Long enough for the
/// scripted segment body to run to completion when it does NOT drain (the
/// RED state), short enough that a regressed fill is observable as a
/// still-blocked return rather than a hung suite.
const DRAIN_PROBE: Duration = Duration::from_secs(2);

/// The gated-tail stand-in for the decided call's tool: on invocation it
/// spawns a TRACKED tail through the run's ONE execution scope — the same
/// registration path every production fire-and-forget tail takes — that
/// holds until the test releases it. The spawn happens DURING the segment
/// (the substitution invokes this tool), so the tail is still in flight
/// when the segment body finishes.
struct GatedTailTool {
    scope: Arc<crate::orchestration::RunExecutionScope>,
    gate: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
}

impl rig::tool::Tool for GatedTailTool {
    const NAME: &'static str = "gated_tail";

    type Error = std::convert::Infallible;
    type Args = FreeformArgs;
    type Output = String;

    fn name(&self) -> String {
        TOOL.to_string()
    }

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: self.name(),
            description: "Test stand-in: spawns a gated tracked tail on the run's scope."
                .to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let gate = self
            .gate
            .lock()
            .expect("the gated tail's gate lock")
            .take()
            .expect("the gated tail spawns once");
        self.scope.spawn_tracked(async move {
            let _ = gate.await;
        });
        Ok(ECHO_TOOL_RESULT.to_string())
    }
}

/// The supervisor-drain rendezvous stand-in frame 10 stages with: the
/// same tracked-tail shape as [`GatedTailTool`], plus the two frame
/// observables — the substitution's spawn moment and the tail's own
/// completion moment.
struct SupervisorGatedTailTool {
    scope: Arc<crate::orchestration::RunExecutionScope>,
    gate: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    spawned: Arc<std::sync::atomic::AtomicUsize>,
    finished: Arc<std::sync::atomic::AtomicUsize>,
}

impl SupervisorGatedTailTool {
    fn new(
        scope: Arc<crate::orchestration::RunExecutionScope>,
        gate: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        spawned: Arc<std::sync::atomic::AtomicUsize>,
        finished: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            scope,
            gate,
            spawned,
            finished,
        }
    }
}

impl rig::tool::Tool for SupervisorGatedTailTool {
    const NAME: &'static str = "supervisor_gated_tail";

    type Error = std::convert::Infallible;
    type Args = FreeformArgs;
    type Output = String;

    fn name(&self) -> String {
        TOOL.to_string()
    }

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: self.name(),
            description: "Test stand-in: spawns a gated tracked tail on the run's scope."
                .to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let gate = self
            .gate
            .lock()
            .expect("the gated tail's gate lock")
            .take()
            .expect("the gated tail spawns once");
        let finished = Arc::clone(&self.finished);
        self.spawned
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.scope.spawn_tracked(async move {
            let _ = gate.await;
            finished.fetch_add(1, std::sync::atomic::Ordering::Release);
        });
        Ok(ECHO_TOOL_RESULT.to_string())
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment re-parks");
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
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

/// A1 (aura#271, card P45; Mike's 2026-09-25 ruling): the resume
/// segment's gate lifecycle events ride the LIVE request id channel —
/// the fresh `req_<uuid>` the resume caller stamped into the World's
/// config and the SSE side subscribes under — never the run owner id.
/// Today `run_segment_borrowed` overwrites `config.request_id` with
/// `run_owner_id(...)` before the orchestrator builds (the gov-500
/// stamp), so the resumed worker's gate publishes its `Requested` under
/// `run:<id>` and a subscriber keyed on the fresh id — exactly the key
/// the web-server handler subscribes for this request — never sees it.
/// RED until the overwrite is deleted: the re-parked call's gate-entry
/// `Requested` must arrive on the fresh channel, naming the fresh
/// ticket's decision id. The `Completed` leg is structurally suppressed
/// on a pending reply (`GateDecision::to_outcome` — a 207 has no
/// terminal outcome), so a re-parking segment emits `Requested` only.
#[tokio::test]
async fn a1_resumed_gate_requested_publishes_on_the_fresh_request_id_channel() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    // The decided tool's recording registration and the sentinel prompt
    // are fixture requirements, mirroring the re-park frame above: the
    // frame pins the event channel only.
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

    let mut events = crate::approval_event_broker::subscribe(REQUEST_ID).await;
    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment re-parks");
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "the freshly gated call re-parks: {segment:?}"
    );

    // The fresh gated call's stored ticket, for the decision-id match.
    let undecided = world
        .store
        .list_pending()
        .await
        .expect("the store lists its undecided approvals");
    let fresh = undecided
        .iter()
        .find(|ticket| {
            ticket
                .request
                .items
                .first()
                .is_some_and(|item| item.tool_name == NEW_TOOL)
        })
        .expect("the re-parked fresh ticket is stored")
        .request
        .decision_id;

    // Drain until the fresh call's own Requested arrives: the shared
    // golden REQUEST_ID also carries OTHER resume machinery's lifecycle
    // publications (consult teardowns, sweeps) from concurrently-running
    // frames, and the fresh decision id is minted inside THIS segment —
    // only the event naming it is this frame's target. Everything else on
    // the channel is skipped.
    let mut requested = None;
    loop {
        match tokio::time::timeout(Duration::from_millis(300), events.recv()).await {
            Ok(Some(crate::approval_event_broker::ApprovalLifecycleEvent::Requested(event)))
                if event.decision_id == fresh.to_string() =>
            {
                requested = Some(event);
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
    let Some(event) = requested else {
        panic!(
            "A1 (aura#271): the resumed gate's Requested for the fresh call ({fresh}) \
             must reach the fresh request id's subscriber ({REQUEST_ID}); today the \
             resume path's config overwrite publishes it under the run owner id and \
             the SSE channel never sees it"
        );
    };
    assert_eq!(
        event.tool_name, NEW_TOOL,
        "the gate-entry Requested on the fresh channel names the re-parked call"
    );
    crate::approval_event_broker::unsubscribe(REQUEST_ID).await;
}

/// A1 regression pin (ruling #4): the re-parked ROW itself keeps the run
/// owner id — the 207 bridge's re-mint (gate.rs `park_207_bridge`) is
/// UNCHANGED by the id-channel split. GREEN today and must stay green
/// after the fill: only the wire body's mint and the live channels move;
/// the parked row's ownership key still names the run.
#[tokio::test]
async fn a1_reparked_row_keeps_the_run_owner_request_id() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment re-parks");
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "the freshly gated call re-parks: {segment:?}"
    );

    let undecided = world
        .store
        .list_pending()
        .await
        .expect("the store lists its undecided approvals");
    let fresh = undecided
        .iter()
        .find(|ticket| {
            ticket
                .request
                .items
                .first()
                .is_some_and(|item| item.tool_name == NEW_TOOL)
        })
        .expect("the re-parked fresh ticket is stored");
    assert_eq!(
        fresh.request.request_id,
        run_owner_id(RUN),
        "A1 ruling #4: the 207 bridge's re-mint keeps parking rows under the run \
         owner id; the id-channel split must not move the ownership key"
    );
}

/// A1 (aura#271, card P45): the resumed MCP manager must be ARMED with
/// the config's fresh request id the way the chat path arms it
/// (`mcp_manager.set_current_request`, factory.rs), and the segment close
/// (`close_segment_mcp`) must cancel under that same fresh id. Today
/// `run_segment_borrowed`'s overwrite hands the orchestrator
/// `config.request_id = run:<run_id>` AND `for_resume_segment` never arms
/// the manager, so resumed MCP calls run untracked and the close cancels
/// under the conflated run owner id. RED until the overwrite is deleted
/// and the arm lands: both observation records must name the World's
/// fresh `REQUEST_ID`. F5 is merged into this frame: the one
/// `McpManager`-level observation seam pins both the arm key and the
/// close key, and the two assertions share one segment drive.
///
/// The world carries one UNREACHABLE HTTP-streamable server (loopback
/// port 1 refuses immediately; the manager.rs precedent): the manager is
/// `Some` — so the arm and the close both run — with zero connected
/// clients. The manager-level call keys are what this frame pins; the
/// per-client fan-out is pinned by the mcp client's own tests.
#[tokio::test]
async fn a1_resumed_segment_arms_and_closes_mcp_under_the_fresh_request_id() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let mut world = world();
    world.config.mcp = Some(aura_config::McpConfig {
        servers: HashMap::from([(
            "unreachable".to_string(),
            aura_config::McpServerConfig::HttpStreamable {
                url: "http://127.0.0.1:1/mcp".to_string(),
                headers: HashMap::new(),
                description: None,
                headers_from_request: HashMap::new(),
                scratchpad: HashMap::new(),
                user_agent: None,
            },
        )]),
        ..Default::default()
    });
    let invocations = Arc::new(Mutex::new(Vec::new()));
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

    crate::mcp::a1_observation::reset();
    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment re-parks");
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "the freshly gated call re-parks: {segment:?}"
    );

    assert_eq!(
        crate::mcp::a1_observation::last_armed(),
        Some(REQUEST_ID.to_string()),
        "A1 (aura#271): for_resume_segment must arm the resumed MCP manager with the \
         config's FRESH request id, the way the chat path arms it; today nothing arms \
         the manager and resumed MCP calls run untracked"
    );
    assert_eq!(
        crate::mcp::a1_observation::last_cancel_key(),
        Some(REQUEST_ID.to_string()),
        "A1 (aura#271): close_segment_mcp must cancel under the config's FRESH request \
         id; today the resume path's config overwrite makes it cancel under the \
         conflated run owner id"
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
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
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "node A's continuation re-parks the first segment: {segment:?}"
    );

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
    let fresh_decision = fresh_ticket.request.decision_id;
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the second segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "the second segment completes: {segment:?}"
    );
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
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
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "the continuation re-parks through the live arm: {segment:?}"
    );
    // The freshly gated call's ticket is the one undecided ticket under the
    // original run id; the decided original left the store.
    let undecided = world
        .store
        .list_pending()
        .await
        .expect("the store lists its undecided approvals");
    assert_eq!(
        undecided.len(),
        1,
        "exactly the fresh ticket remains undecided"
    );
    let fresh_decision = undecided[0].request.decision_id;

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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the second segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "the second segment completes: {segment:?}"
    );
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
}

/// A recorded approval executes exactly once through the worker's gated
/// pipeline, the real result reaches the model in the reconstructed
/// context (the outcome package the rebuilt history carries — the R2
/// wire pair rides the RE-PARKED segment only, R6), and the coordinator
/// continuation finishes the run over a scripted respond_directly. The
/// park placeholder appears nowhere on the wire.
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "the segment completes: {segment:?}"
    );
    {
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
    }
}

/// A recorded denial steers: the call never executes, the live denial text
/// and its reason ride the continuation context verbatim in place of the
/// placeholder (the rebuilt history's outcome package — the R2 wire pair
/// rides the RE-PARKED segment only, R6), and the coordinator continuation
/// finishes the run over a scripted respond_directly; the worker
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "the segment completes: {segment:?}"
    );
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
    assert!(
        !context.to_string().contains(PARK_SENTINEL),
        "the park placeholder must not survive a decided resume's context"
    );
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("an execution failure is result text, never a segment fault");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "an execution failure completes the segment: {segment:?}"
    );
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
    assert!(
        !context.to_string().contains(ECHO_TOOL_RESULT)
            && !context.to_string().contains(PARK_SENTINEL),
        "no fabricated success and no placeholder in the rebuilt context"
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

    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("a denial needs no identity: the segment steers, never faults");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "a denial completes the segment: {segment:?}"
    );
    assert!(
        invocations.lock().expect("tool invocation log").is_empty(),
        "the denied call never executes"
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
    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
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
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "the node's continuation re-parks the first segment: {segment:?}"
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the second segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "the second segment completes: {segment:?}"
    );
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

    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let fault = run_segment_live(grant, &world.config, &HashMap::new())
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the pivot segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
        "the pivot segment completes: {segment:?}"
    );
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the pivot segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the pivot segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the resumed run drives to completion");
    let ResumeStreamEnd::Completed { final_answer } = segment else {
        panic!("expected a completed segment, got {segment:?}")
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
    assert_eq!(
        final_answer, COORD_FINAL_ANSWER,
        "the coordinator finishes its turn naturally after the workers (R6)"
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the resumed run completes after the worker failure");
    let ResumeStreamEnd::Completed { final_answer } = segment else {
        panic!("expected a completed segment, got {segment:?}")
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
    // The load-bearing re-plan pin: the failure reached the loop's
    // decision context. The continuation request the coordinator
    // received carries the failed node as plan state and records the
    // failure under the first FRESH iteration (the stream shape's fresh
    // cycle counter seeds iteration 1; the checkpoint's historical
    // iteration is evidence-only).
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
            "- Iteration 1: \"Gated apply\" (worker: operations) — [soft_failure] {FAILED_TEXT}"
        )),
        "the failure history records the failure under the first fresh iteration: {prompt}"
    );
    // The loop-continuation leg: the coordinator actually continued past
    // the failure — a replacement build or a final answer, whichever it
    // chose.
    let probe_count = probe_invocations
        .lock()
        .expect("replacement probe invocation log")
        .len();
    let answered = final_answer == COORD_FINAL_ANSWER;
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the resumed run completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the continuation re-parks on the fresh gated call");
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("node B's continuation re-parks the segment");
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "node B's continuation re-parks the segment: {segment:?}"
    );
    // The freshly gated call's ticket is the one undecided ticket.
    let undecided = world
        .store
        .list_pending()
        .await
        .expect("the store lists its undecided approvals");
    assert_eq!(
        undecided.len(),
        1,
        "exactly one fresh undecided ticket remains"
    );
    let fresh_decision = undecided[0].request.decision_id;

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
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the second segment completes");
    assert!(
        matches!(segment, ResumeStreamEnd::Completed { .. }),
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

// =====================================================================
// E4-R goldens (the RESERVATION wave, aura/P57): the reserved
// rename-back and conversion seams (SKELETON rows 10/11 — RED today,
// each raising its documented todo), the ordered entry (reserve
// precedes the consult — RED behavioral today), and the pending-outcome
// release regression guard.
// =====================================================================

use super::claim::ClaimResumeFault;
use std::sync::atomic::{AtomicBool, Ordering};

/// A one-shot stalling store double: every operation forwards to the
/// wrapped file store exactly as the E3 fault double forwards to its
/// inner store, EXCEPT `read_or_expire` — the cutover consult's per-member
/// read — which stalls exactly the FIRST call: signals arrival, awaits the
/// release gate, then forwards the read. Every later `read_or_expire`
/// forwards immediately.
struct StallingReadStore {
    inner: Arc<crate::session_store::FileApprovalStore>,
    /// Notified when the first stalling `read_or_expire` arrives inside the
    /// consult.
    arrived: Arc<tokio::sync::Notify>,
    /// The one-shot release the stalling `read_or_expire` awaits.
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// Whether the first `read_or_expire` already stalled.
    stalled: AtomicBool,
}

#[async_trait::async_trait]
impl ApprovalStore for StallingReadStore {
    async fn register(
        &self,
        parked: ParkedApproval,
    ) -> Result<(), crate::session_store::SessionStoreError> {
        self.inner.register(parked).await
    }

    async fn mark_acknowledged(
        &self,
        id: &DecisionId,
    ) -> Result<crate::session_store::AcknowledgeOutcome, crate::session_store::SessionStoreError>
    {
        self.inner.mark_acknowledged(id).await
    }

    async fn get(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ParkedApproval>, crate::session_store::SessionStoreError> {
        self.inner.get(id).await
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        expected_authority: crate::hitl::ApprovalAuthority,
        decision: ResolvedDecision,
    ) -> Result<(), crate::hitl::ResolveError> {
        self.inner.resolve(id, expected_authority, decision).await
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ResolvedDecision>, crate::session_store::SessionStoreError> {
        self.inner.decision(id).await
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), crate::session_store::SessionStoreError> {
        self.inner.remove(id).await
    }

    async fn cancel_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, crate::session_store::SessionStoreError> {
        self.inner.cancel_request(request_id).await
    }

    async fn list_pending(
        &self,
    ) -> Result<Vec<ParkedApproval>, crate::session_store::SessionStoreError> {
        self.inner.list_pending().await
    }

    async fn read_or_expire(
        &self,
        id: &DecisionId,
        expected_authority: crate::hitl::ApprovalAuthority,
    ) -> Result<crate::hitl::ApprovalRead, crate::session_store::SessionStoreError> {
        if !self.stalled.swap(true, Ordering::SeqCst) {
            self.arrived.notify_one();
            let release = self
                .release
                .lock()
                .expect("the stalling store's release gate")
                .take()
                .expect("the stalling read_or_expire's release is supplied exactly once");
            let _ = release.await;
        }
        self.inner.read_or_expire(id, expected_authority).await
    }

    async fn retained_rows(
        &self,
    ) -> Result<Vec<crate::session_store::RetainedApproval>, crate::session_store::SessionStoreError>
    {
        self.inner.retained_rows().await
    }
}

/// The gate the ordered-entry golden holds against its stalling world.
struct StallingReadGate {
    /// Notified when the first POST's consult reaches its store read.
    arrived: Arc<tokio::sync::Notify>,
    /// The release that lets the stalled consult proceed.
    release: tokio::sync::oneshot::Sender<()>,
}

/// The ordered-entry world: the default world's store and worker surface
/// recomposed over the stalling store double, with a HITL closure in the
/// unreachable-URL poll-route shape the identity frames use — the consult
/// never reaches the network, and the run's consult read stalls on the
/// double for exactly one call.
fn world_over_stalling_read() -> (World, StallingReadGate) {
    let dir = tempfile::tempdir().expect("temp memory root");
    std::fs::create_dir_all(dir.path().join("approvals")).expect("approval dir");
    let store = Arc::new(
        crate::session_store::FileApprovalStore::open(dir.path().join("approvals"))
            .expect("file approval store"),
    );
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let arrived = Arc::new(tokio::sync::Notify::new());
    let double = StallingReadStore {
        inner: Arc::clone(&store),
        arrived: Arc::clone(&arrived),
        release: Mutex::new(Some(held)),
        stalled: AtomicBool::new(false),
    };
    let registry = PendingApprovals::with_backend(
        std::sync::Arc::new(double) as Arc<dyn crate::session_store::ApprovalStore>,
        Arc::new(crate::session_store::InMemoryEventBus::new())
            as Arc<dyn crate::session_store::EventBus>,
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
        hitl: Some(hitl_closure(&registry)),
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
    let world = World {
        dir,
        memory_dir,
        store,
        registry,
        config,
        claims: ResumeClaimTable::new(),
        _receiver: tokio::spawn(async {}),
    };
    (world, StallingReadGate { arrived, release })
}

/// The unreachable-URL poll-route HITL closure the identity frames build
/// with: same shape, no receiver to connect.
fn hitl_closure(registry: &PendingApprovals) -> crate::hitl::HitlRuntime {
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
}

/// A resuming document is renamed back to its parked name under the held
/// reservation: the run is live while the fenced seam runs, the blocking
/// rename tail completes with the parked path restored and the resuming
/// path gone, and dropping the lease reference releases the run.
#[tokio::test]
async fn reservation_rename_back_restores_the_parked_name_under_the_held_reservation() {
    let world = world();
    stage_resuming_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let path = ValidatedResumePath::parse(SESSION, RUN).expect("golden path validates");
    let docs = ResumeDocuments::for_path(&path, &world.memory_dir);
    let reservation = world
        .claims
        .reserve(&path.run)
        .expect("the fresh run reserves");
    assert!(
        world.claims.is_live(&path.run),
        "the reserved run is live while the fenced rename-back runs"
    );

    world
        .claims
        .rename_back_under_reservation(&reservation, &docs)
        .await
        .expect("the fenced rename-back completes");

    assert!(
        parked_document_path(&world).exists(),
        "the fenced rename-back restored the parked name"
    );
    assert!(
        !resuming_document_path(&world).exists(),
        "the resuming name is gone after the fenced rename-back"
    );
    drop(reservation);
    assert!(
        !world.claims.is_live(&path.run),
        "dropping the lease reference releases the run"
    );
}

/// A rename the filesystem refuses answers the availability arm while the
/// reservation stays held: a directory at the rename destination makes the
/// rename fail deterministically, and the fenced seam's fault is the
/// availability arm, never the live or internal arm.
#[tokio::test]
async fn reservation_rename_back_maps_a_failed_rename_to_unavailable() {
    let world = world();
    stage_resuming_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;
    let path = ValidatedResumePath::parse(SESSION, RUN).expect("golden path validates");
    let docs = ResumeDocuments::for_path(&path, &world.memory_dir);
    let reservation = world
        .claims
        .reserve(&path.run)
        .expect("the fresh run reserves");
    // A directory occupying the parked name makes the resuming → parked
    // rename fail deterministically on every platform: the destination
    // must be replaced, not descended into.
    std::fs::create_dir_all(parked_document_path(&world)).expect("stage the destination obstacle");

    match world
        .claims
        .rename_back_under_reservation(&reservation, &docs)
        .await
    {
        Err(ClaimResumeFault::Unavailable(_)) => {}
        other => panic!("a filesystem-refused rename answers the availability arm, got {other:?}"),
    }
    // The directory stays in place; the TempDir cleanup removes it.
}

/// A rename with neither name on disk answers the availability arm: the
/// resuming name is absent and no parked name exists to have lost a race,
/// so there is no live hold and no internal fault to answer instead.
#[tokio::test]
async fn reservation_rename_back_maps_a_missing_pair_to_unavailable() {
    let world = world();
    let path = ValidatedResumePath::parse(SESSION, RUN).expect("golden path validates");
    let docs = ResumeDocuments::for_path(&path, &world.memory_dir);
    let reservation = world
        .claims
        .reserve(&path.run)
        .expect("the fresh run reserves");

    match world
        .claims
        .rename_back_under_reservation(&reservation, &docs)
        .await
    {
        Err(ClaimResumeFault::Unavailable(_)) => {}
        other => panic!("a missing document pair answers the availability arm, got {other:?}"),
    }
}

/// The held reservation converts into the grant and nothing else: the
/// parked document renames to its resuming name under that same
/// reservation — no second acquisition, no ownerless gap — and the grant
/// assembles owning the run's ONE execution scope, the Arc its accessor
/// hands out twice being one shared scope, while the grant lives the run
/// stays reserved and its drop releases it.
#[tokio::test]
async fn reservation_convert_reserved_grants_owning_the_held_reservation_and_one_scope() {
    let world = world();
    register_decided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let path = ValidatedResumePath::parse(SESSION, RUN).expect("golden path validates");
    let session = path.session.clone();
    let run = path.run.clone();
    let docs = ResumeDocuments::for_path(&path, &world.memory_dir);
    let reserved = ReservedEvaluation::new(
        world.claims.reserve(&run).expect("the fresh run reserves"),
        docs,
        parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    );

    let grant = convert_reserved(
        &world.claims,
        reserved,
        session,
        run.clone(),
        Arc::new(super::super::RecordedDecisions::default()),
        vec![],
    )
    .await
    .expect("the held reservation converts into the grant");

    assert!(
        resuming_document_path(&world).exists(),
        "the consuming conversion renamed the parked document to its resuming name"
    );
    assert!(
        !parked_document_path(&world).exists(),
        "the parked name is gone after the consuming conversion"
    );
    assert!(
        world.claims.is_live(&run),
        "the run stays reserved while the grant lives"
    );
    assert!(
        Arc::ptr_eq(&grant.execution_scope(), &grant.execution_scope()),
        "the grant's scope accessor clones the ONE scope Arc, never a fresh scope"
    );
    drop(grant);
    assert!(
        !world.claims.is_live(&run),
        "the grant dropping releases the reservation"
    );
}

/// A refused conversion releases the reservation with no execution: a
/// directory at the resuming name makes the conversion's rename fail, the
/// fault answers the availability arm, and the run is not live after the
/// refusal — no execution may ride a refused conversion.
#[tokio::test]
async fn reservation_convert_reserved_maps_a_failed_rename_to_unavailable_and_releases() {
    let world = world();
    register_decided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;

    let path = ValidatedResumePath::parse(SESSION, RUN).expect("golden path validates");
    let session = path.session.clone();
    let run = path.run.clone();
    let docs = ResumeDocuments::for_path(&path, &world.memory_dir);
    let reserved = ReservedEvaluation::new(
        world.claims.reserve(&run).expect("the fresh run reserves"),
        docs,
        parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    );
    // A directory occupying the resuming name makes the parked → resuming
    // rename fail deterministically on every platform.
    std::fs::create_dir_all(resuming_document_path(&world))
        .expect("stage the resuming-name obstacle");

    match convert_reserved(
        &world.claims,
        reserved,
        session,
        run.clone(),
        Arc::new(super::super::RecordedDecisions::default()),
        vec![],
    )
    .await
    {
        Err(ClaimResumeFault::Unavailable(_)) => {}
        other => {
            panic!("a filesystem-refused conversion answers the availability arm, got {other:?}")
        }
    }
    assert!(
        !world.claims.is_live(&run),
        "a refused conversion releases the reservation with no execution"
    );
}

/// The ordered entry fences the consult: while a first POST is blocked
/// inside its consult's store read, a second POST of the same run answers
/// the running row — the run's reservation was pinned at step 2, before
/// the consult, so no second evaluation can consult unreserved — and the
/// first POST, released, answers the parked row. No ownerless gap.
#[tokio::test]
async fn reservation_second_post_during_the_consult_answers_running() {
    let (world, gate) = world_over_stalling_read();
    register_undecided(&world).await;
    publish_document(
        &world,
        &parked_document(FUTURE_STAMP, matching_fingerprint(&world), None, vec![]),
    )
    .await;
    let world = Arc::new(world);

    let first = tokio::spawn({
        let world = Arc::clone(&world);
        async move { evaluate_resume(evaluation(&world, false, None)).await }
    });
    gate.arrived.notified().await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the second post of a held-back run refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "running",
            "detail": "another resume holds this run",
            "blocking": [],
        }),
    );

    gate.release
        .send(())
        .expect("the stalling consult's gate releases");
    let first_refusal = first
        .await
        .expect("the first post's task completes")
        .expect_err("the undecided first post answers the parked row");
    assert_conflict(
        first_refusal,
        json!({
            "code": "parked",
            "detail": "calls still await a decision",
            "blocking": [entry(decision(), TOOL, TICKET_STAMP)],
        }),
    );
}

/// A pending outcome releases the reservation: the undecided-run POST
/// answers the parked row and the run is not live after the refusal — no
/// reservation may leak on the release-with-no-execution path.
#[tokio::test]
async fn reservation_pending_outcome_releases_the_reservation() {
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
            "blocking": [entry(decision(), TOOL, TICKET_STAMP)],
        }),
    );
    let path = ValidatedResumePath::parse(SESSION, RUN).expect("golden path validates");
    assert!(
        !world.claims.is_live(&path.run),
        "a refused outcome releases the reservation with no execution"
    );
}

// =====================================================================
// E7-R goldens (the CONSULT wave, aura/P57): the contract's "Outcome and
// expiry seam" as RED goldens for the E7 production consult cutover.
// Per-member `read_or_expire`; a timeout ADDRESSES one call through
// `Addressed`/`TimedOut`; `blocking[]` carries each call's ACTUAL
// deadline; the run-wide window treatment and the second racy projection
// retire. Tests 2 and 8 are regression guards (green today); the rest
// must fail for the documented behavioral reasons, never compilation.
// =====================================================================

use crate::hitl::{AddressedApproval, DecisionRoute, WebhookClient};
use crate::orchestration::RecordedDecisions;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper};
use std::sync::atomic::AtomicUsize;

/// The expired row's detail: the consult's own prose, not under test for
/// redesign — pinned verbatim here and by the retired-window guard.
const EXPIRED_DETAIL: &str = "the decision window closed before every pending call was decided";

/// The two-call bundle's member deadlines in the per-call-deadline frames
/// (tests 3): fixed whole-second stamps, distinct from each other and from
/// the document's retention stamp, so an entry stamped with the retention
/// stamp cannot pass as its call's own deadline.
const ROW_DEADLINE_A: &str = "2090-01-01T00:00:00Z";
const ROW_DEADLINE_B: &str = "2091-01-01T00:00:00Z";

/// One node's ticket with a caller-supplied approval deadline — the fixture
/// voice for rows whose OWN window the consult must honor ("per-member
/// read_or_expire"). Same shape `node_approval` builds, so the stored row
/// is exactly what a park producer writes, only its window differs.
#[allow(clippy::too_many_arguments)]
fn node_approval_expiring(
    decision_id: DecisionId,
    run: &str,
    task_id: usize,
    tool: &str,
    args: &Value,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> ParkedApproval {
    let mut approval = node_approval(decision_id, run, task_id, tool, args);
    approval.expires_at = expires_at;
    approval
}

/// The two-call, one-awaiting-node bundle: node task 3 carrying the standard
/// first call plus the pivot-shape second call (a distinct `kubectl_*` tool,
/// its own arguments, its own call id and decision) — the bundle shape the
/// timed-out-addressing (test 1) and retention-expired all-addressed
/// (test 4) frames drive.
fn two_call_bundle_document(world: &World, expires_at: &str) -> ParkedRun {
    let mut document = parked_document(expires_at, matching_fingerprint(world), None, Vec::new());
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

/// The standard fixture's sentinel document carrying a caller-supplied
/// retention stamp: the single-awaiting-node shape the expired-row frames
/// (tests 5 and 8) publish, without touching the shared helpers.
fn sentinel_document_with_retention(world: &World, expires_at: &str) -> ParkedRun {
    let mut document = parked_document(expires_at, matching_fingerprint(world), None, Vec::new());
    let node = document
        .plan
        .tasks
        .first_mut()
        .expect("the skeleton carries one awaiting node");
    node.current_prompt = Some(sentinel_prompt());
    document
}

/// A counting store double: every operation forwards to the wrapped file
/// store exactly as the E4-R fault double forwards to its inner store,
/// EXCEPT it counts the consult's reads — `read_or_expire` (the cutover's
/// per-member read) and the interim consult's reads (`try_parked` → `get`,
/// `recorded_decision` → `decision`). Test 6's frame.
struct CountingStore {
    inner: Arc<crate::session_store::FileApprovalStore>,
    gets: AtomicUsize,
    decisions: AtomicUsize,
    read_or_expires: AtomicUsize,
}

#[async_trait::async_trait]
impl ApprovalStore for CountingStore {
    async fn register(
        &self,
        parked: ParkedApproval,
    ) -> Result<(), crate::session_store::SessionStoreError> {
        self.inner.register(parked).await
    }

    async fn mark_acknowledged(
        &self,
        id: &DecisionId,
    ) -> Result<crate::session_store::AcknowledgeOutcome, crate::session_store::SessionStoreError>
    {
        self.inner.mark_acknowledged(id).await
    }

    async fn get(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ParkedApproval>, crate::session_store::SessionStoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(id).await
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        expected_authority: crate::hitl::ApprovalAuthority,
        decision: ResolvedDecision,
    ) -> Result<(), crate::hitl::ResolveError> {
        self.inner.resolve(id, expected_authority, decision).await
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ResolvedDecision>, crate::session_store::SessionStoreError> {
        self.decisions.fetch_add(1, Ordering::SeqCst);
        self.inner.decision(id).await
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), crate::session_store::SessionStoreError> {
        self.inner.remove(id).await
    }

    async fn cancel_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, crate::session_store::SessionStoreError> {
        self.inner.cancel_request(request_id).await
    }

    async fn list_pending(
        &self,
    ) -> Result<Vec<ParkedApproval>, crate::session_store::SessionStoreError> {
        self.inner.list_pending().await
    }

    async fn read_or_expire(
        &self,
        id: &DecisionId,
        expected_authority: crate::hitl::ApprovalAuthority,
    ) -> Result<crate::hitl::ApprovalRead, crate::session_store::SessionStoreError> {
        self.read_or_expires.fetch_add(1, Ordering::SeqCst);
        self.inner.read_or_expire(id, expected_authority).await
    }

    async fn retained_rows(
        &self,
    ) -> Result<Vec<crate::session_store::RetainedApproval>, crate::session_store::SessionStoreError>
    {
        self.inner.retained_rows().await
    }
}

/// The counting world: the default world's config over a registry whose
/// store is the counting double — the consult's reads pass through it, and
/// the test pins the counts through the returned `Arc`.
fn world_over_counting() -> (World, Arc<CountingStore>) {
    let dir = tempfile::tempdir().expect("temp memory root");
    std::fs::create_dir_all(dir.path().join("approvals")).expect("approval dir");
    let store = Arc::new(
        crate::session_store::FileApprovalStore::open(dir.path().join("approvals"))
            .expect("file approval store"),
    );
    let double = Arc::new(CountingStore {
        inner: Arc::clone(&store),
        gets: AtomicUsize::new(0),
        decisions: AtomicUsize::new(0),
        read_or_expires: AtomicUsize::new(0),
    });
    let registry = PendingApprovals::with_backend(
        std::sync::Arc::clone(&double) as Arc<dyn crate::session_store::ApprovalStore>,
        Arc::new(crate::session_store::InMemoryEventBus::new())
            as Arc<dyn crate::session_store::EventBus>,
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
        hitl: Some(hitl_closure(&registry)),
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
    let world = World {
        dir,
        memory_dir,
        store,
        registry,
        config,
        claims: ResumeClaimTable::new(),
        _receiver: tokio::spawn(async {}),
    };
    (world, double)
}

/// Register node B's ticket with a caller-supplied deadline — the fixture
/// voice for the bundle's second member whose own window differs from the
/// retention stamp. (Node B: task 4, distinct tool, its own arguments.)
async fn register_undecided_b_expiring(world: &World, expires_at: chrono::DateTime<chrono::Utc>) {
    world
        .registry
        .register_durable(node_approval_expiring(
            decision_b(),
            RUN,
            4,
            TOOL_B,
            &call_args_b(),
            expires_at,
        ))
        .await
        .expect("register node B's expiring approval");
}

/// A poll webhook route whose webhook is unreachable, so a fall-through to
/// the route fails closed rather than hanging — the recorded-consult shape
/// the gate's own tests build. A recorded hit short-circuits before the
/// route; it never sees this.
fn golden_discard_route() -> Arc<DecisionRoute> {
    Arc::new(DecisionRoute::Webhook {
        client: WebhookClient::new(
            reqwest::Client::new(),
            // Discard port: nothing listens, so the POST fails closed.
            aura_config::WebhookUrl::new("http://127.0.0.1:9").unwrap(),
        ),
        registry: PendingApprovals::new(),
        timeout: Duration::from_secs(2),
        egress_capture: Ok(()),
    })
}

/// Two-call bundle on one awaiting node: call A decided (approved) in the
/// store; call B's parked row registered with a deadline already past (the
/// store's `read_or_expire` therefore addresses it `TimedOut`); the caller's
/// `now` sits inside the document's retention stamp. The consult must
/// address B through its per-member `read_or_expire` and grant the ready
/// bundle, carrying A as `Decided` and B as `TimedOut { deadline }` in the
/// recorded set, with only A in the consumed id list.
///
/// RED today: the interim consult reads `try_parked` + `recorded_decision`,
/// sees B undecided, and refuses (`409 parked`); it never calls
/// `read_or_expire`.
#[tokio::test]
async fn consult_a_timed_out_member_addresses_its_call_and_the_ready_bundle_resumes() {
    let world = world();
    register_decided(&world).await;
    let b_expires = chrono::Utc::now() - chrono::Duration::hours(1);
    world
        .registry
        .register_durable(node_approval_expiring(
            decision_pivot_2(),
            RUN,
            3,
            TOOL_B,
            &call_args_b(),
            b_expires,
        ))
        .await
        .expect("register call B's parked row with its past deadline");
    publish_document(&world, &two_call_bundle_document(&world, FUTURE_STAMP)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect(
            "the addressed bundle must resume READY: call B's stored row deadline is \
             past, so the consult's per-member read_or_expire must address it \
             TimedOut — never the interim 409 parked",
        );

    // The recorded set carries A as Decided and B as TimedOut { deadline }.
    let recorded = grant.recorded_decisions();
    match recorded.take(&CallKey::new(3, TOOL, &call_args())) {
        Some(AddressedApproval::Decided(ResolvedDecision::Approved { .. })) => {}
        other => panic!("call A addresses Decided: {other:?}"),
    }
    match recorded.take(&CallKey::new(3, TOOL_B, &call_args_b())) {
        Some(AddressedApproval::TimedOut { deadline }) => assert_eq!(
            deadline, b_expires,
            "the timed-out member's deadline is exactly its stored row deadline"
        ),
        other => panic!("call B addresses TimedOut {{ deadline }}: {other:?}"),
    }

    // The consumed id list contains A and NOT B: a timeout consumes no
    // decision from the store.
    let consumed = grant.consumed_decisions();
    assert!(
        consumed.contains(&decision()),
        "the decided member is consumed: {consumed:?}"
    );
    assert!(
        !consumed.contains(&decision_pivot_2()),
        "the timed-out member is never a consumed decision: {consumed:?}"
    );
    drop(grant);
}

/// Guard: the recordable `TimedOut` arm, consumed through the gate's
/// recorded-decision path (`recorded_pre_call` / `TerminalGateDecision::
/// TimedOut`), yields exactly `tool call denied: approval timed out` — the
/// exact shared mapping (`approval_result_to_pre_call`), not a paraphrase
/// and never a fabricated denial. The consult cutover must feed its
/// addressed timeouts through THIS mapping unchanged.
#[tokio::test]
async fn consult_feeds_the_exact_shared_timeout_feedback_at_consumption() {
    let recorded = Arc::new(RecordedDecisions::default());
    recorded.push(
        CallKey::new(3, TOOL, &call_args()),
        AddressedApproval::TimedOut {
            deadline: chrono::DateTime::parse_from_rfc3339(PAST_STAMP)
                .expect("golden stamp parses")
                .with_timezone(&chrono::Utc),
        },
    );
    let gate = crate::hitl::HitlApprovalWrapper::new(
        Arc::from([aura_config::GlobPattern::new("kubectl_*").unwrap()]),
        golden_discard_route(),
        AgentScope::Single { session_id: None },
        REQUEST_ID.to_string(),
        "test-agent".to_string(),
        "golden-instance".to_string(),
    )
    .with_recorded_decisions(recorded);
    let mut ctx = ToolCallContext::new(TOOL);
    ctx.task_id = Some(3);

    let err = gate
        .pre_call(&call_args(), &ctx)
        .await
        .expect_err("the recorded TimedOut arm fails the call closed");
    let rig::tool::ToolError::ToolCallError(inner) = err else {
        panic!("the timeout mapping is a tool call error: {err:?}")
    };
    assert_eq!(
        inner.to_string(),
        "tool call denied: approval timed out",
        "the exact shared TerminalGateDecision::TimedOut wording — never a \
         paraphrase, never a fabricated denial"
    );
}

/// Two pending calls whose stored rows carry DIFFERENT deadlines; the
/// caller's `now` sits inside retention. The `409 parked` row must carry
/// each `blocking[]` entry's OWN stored row deadline — and neither entry
/// may equal the document's retention stamp (the run-wide retirement).
///
/// RED today: every entry carries `document.retention_expires_at`.
#[tokio::test]
async fn consult_blocking_entries_carry_each_calls_own_deadline() {
    let world = world();
    world
        .registry
        .register_durable(node_approval_expiring(
            decision(),
            RUN,
            3,
            TOOL,
            &call_args(),
            chrono::DateTime::parse_from_rfc3339(ROW_DEADLINE_A)
                .expect("golden stamp parses")
                .with_timezone(&chrono::Utc),
        ))
        .await
        .expect("register member A with its own window");
    let b_stamp = chrono::DateTime::parse_from_rfc3339(ROW_DEADLINE_B)
        .expect("golden stamp parses")
        .with_timezone(&chrono::Utc);
    // Registered LATER than member A, with its own (different) window.
    register_undecided_b_expiring(&world, b_stamp).await;
    publish_document(&world, &two_node_sentinel_document(&world)).await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the undecided members refuse parked");
    assert_conflict(
        refusal,
        json!({
            "code": "parked",
            "detail": "calls still await a decision",
            "blocking": [
                entry(decision(), TOOL, ROW_DEADLINE_A),
                entry(decision_b(), TOOL_B, ROW_DEADLINE_B),
            ],
        }),
    );
}

/// Both members addressed (one decided, one timed-out), the caller's `now`
/// past the document's retention stamp. The checkpoint must still expire:
/// the terminal `409 expired` row (both members addressed, so nothing
/// blocks), the parked checkpoint unlinked, the durable addressed terminal
/// records retained as evidence, and a retried resume of the same run
/// answers the absent row.
///
/// RED today: the consult pins the addressed bundle to members addressed
/// `TimedOut` only after the cutover — the interim consult carries the
/// addressed member as an outstanding undecided call, so the expired row's
/// blocking set is not empty.
#[tokio::test]
async fn consult_a_retention_expired_all_addressed_checkpoint_still_expires_and_tears_down() {
    let world = world();
    register_decided(&world).await;
    let b_expires = chrono::Utc::now() - chrono::Duration::hours(1);
    world
        .registry
        .register_durable(node_approval_expiring(
            decision_pivot_2(),
            RUN,
            3,
            TOOL_B,
            &call_args_b(),
            b_expires,
        ))
        .await
        .expect("register call B's parked row with its past deadline");
    // The caller's `now` is the real clock: past PAST_STAMP retention.
    publish_document(&world, &two_call_bundle_document(&world, PAST_STAMP)).await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the retention-expired run expires even all-addressed");
    assert_conflict(
        refusal,
        json!({
            "code": "expired",
            "detail": "the decision window closed before every pending call was decided",
            "blocking": [],
        }),
    );
    assert!(
        !parked_document_path(&world).exists() && !resuming_document_path(&world).exists(),
        "the expired teardown unlinked the parked checkpoint"
    );
    // Retained-evidence semantics: the durable terminal records survive the
    // resume-path expired teardown and stay readable. A was decided through
    // the store and B timed out on its own stored deadline; both wrote a
    // durable decision file, and the file store's cancel sweep excludes any
    // id whose decision file is present (session_store/file.rs
    // `cancel_request_sync`'s `stale_decided` branch), so `try_parked` still
    // answers `Some` from the decision file.
    for id in [decision(), decision_pivot_2()] {
        assert!(
            world
                .registry
                .try_parked(&id)
                .await
                .expect("the store reads")
                .is_some(),
            "the durable terminal record for {id} remains retained evidence"
        );
    }
    let retry = evaluate_resume(evaluation(&world, false, None)).await;
    assert!(
        matches!(retry, Err(ResumeRefusal::DocumentAbsent)),
        "a retried resume of the same run answers the absent row: {retry:?}"
    );
}

/// One member pending INSIDE its own window (its stored row deadline in the
/// future on the store clock), the caller's `now` past the document's
/// retention stamp. The terminal `409 expired` row's single `blocking[]`
/// entry carries that call's own (future) deadline — not the past retention
/// stamp.
///
/// RED today: the entry carries the retention stamp.
#[tokio::test]
async fn consult_expired_row_blocks_with_each_calls_actual_deadline() {
    let world = world();
    world
        .registry
        .register_durable(node_approval_expiring(
            decision(),
            RUN,
            3,
            TOOL,
            &call_args(),
            chrono::DateTime::parse_from_rfc3339(FUTURE_STAMP)
                .expect("golden stamp parses")
                .with_timezone(&chrono::Utc),
        ))
        .await
        .expect("register the member pending inside its own window");
    publish_document(
        &world,
        &sentinel_document_with_retention(&world, PAST_STAMP),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the retention-expired run refuses");
    assert_conflict(
        refusal,
        json!({
            "code": "expired",
            "detail": "the decision window closed before every pending call was decided",
            "blocking": [entry(decision(), TOOL, FUTURE_STAMP)],
        }),
    );
}

/// A consult ending in the `409 parked` row reads its bundle members
/// EXACTLY once each through `read_or_expire`, and the interim consult
/// reads (`try_parked`, `recorded_decision`) fire ZERO times for the member
/// consult.
///
/// RED today: zero `read_or_expire` calls — the interim consult reads
/// `try_parked` + `recorded_decision` per member instead.
#[tokio::test]
async fn consult_reads_each_member_exactly_once() {
    let (world, counts) = world_over_counting();
    register_undecided(&world).await;
    let b_expires = chrono::Utc::now() + chrono::Duration::hours(1);
    world
        .registry
        .register_durable(node_approval_expiring(
            decision_b(),
            RUN,
            4,
            TOOL_B,
            &call_args_b(),
            b_expires,
        ))
        .await
        .expect("register member B undecided");
    publish_document(&world, &two_node_sentinel_document(&world)).await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the undecided members answer the parked row");
    match &refusal {
        ResumeRefusal::Conflict(row) => {
            assert!(
                matches!(row.code(), ConflictCode::Parked),
                "the consult ends parked: {row:?}"
            );
        }
        other => panic!("expected the parked row, got {other:?}"),
    }

    let read_or_expires = counts.read_or_expires.load(Ordering::SeqCst);
    assert_eq!(
        read_or_expires, 2,
        "EXACTLY ONE read_or_expire per bundle member (2 members); RED today: \
         the interim consult never calls read_or_expire at all"
    );
    assert_eq!(
        counts.gets.load(Ordering::SeqCst),
        0,
        "the interim try_parked reads must fire ZERO times for the member consult"
    );
    assert_eq!(
        counts.decisions.load(Ordering::SeqCst),
        0,
        "the interim recorded_decision reads must fire ZERO times for the member consult"
    );
}

/// Gate A round-1 repair regression: a bundle whose FIRST member's ticket is
/// missing and whose LATER member is present but identity-mismatched must
/// answer the MISMATCH row even past the document's retention stamp — the
/// present-row mismatch outranks expiry — and the loop must read EVERY member
/// exactly once. The early `Missing`-past-retention return masked the later
/// present mismatch and short-circuited member B's read.
///
/// Fixture: the two-call bundle, member order MISSING FIRST (call A has no
/// store row at all), then call B with a PRESENT row naming another run (its
/// identity validation is the mismatch); the caller's `now` (the real clock)
/// sits past the document's PAST_STAMP retention.
///
/// RED at the fill state: member A's missing row past retention returns the
/// expired row immediately, so B's present mismatch is never observed and B
/// is never read.
#[tokio::test]
async fn consult_a_present_mismatch_outranks_expiry_regardless_of_member_order() {
    let (world, counts) = world_over_counting();
    // Member A (the FIRST bundle member) has NO store row at all.
    // Member B (the LATER member) is present but names another run, so its
    // identity validation is the mismatch the consult must report.
    world
        .registry
        .register_durable(node_approval_expiring(
            decision_pivot_2(),
            OTHER_RUN,
            3,
            TOOL_B,
            &call_args_b(),
            chrono::DateTime::parse_from_rfc3339(FUTURE_STAMP)
                .expect("golden stamp parses")
                .with_timezone(&chrono::Utc),
        ))
        .await
        .expect("register member B's borrowed-run row");
    // The caller's `now` is the real clock: past PAST_STAMP retention.
    publish_document(&world, &two_call_bundle_document(&world, PAST_STAMP)).await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the later present mismatch refuses, never the expired row");
    assert_conflict(
        refusal,
        json!({
            "code": "mismatch",
            "detail": format!(
                "approval {PIVOT_DECISION_2} belongs to run {OTHER_RUN}, not this run"
            ),
            "blocking": [],
        }),
    );

    let read_or_expires = counts.read_or_expires.load(Ordering::SeqCst);
    assert_eq!(
        read_or_expires, 2,
        "BOTH members are read through read_or_expire exactly once each, even \
         though the FIRST is missing; RED at the fill state: the early \
         Missing-past-retention return reads only member A (count 1)"
    );
    assert_eq!(
        counts.gets.load(Ordering::SeqCst),
        0,
        "the interim try_parked reads must fire ZERO times for the member consult"
    );
    assert_eq!(
        counts.decisions.load(Ordering::SeqCst),
        0,
        "the interim recorded_decision reads must fire ZERO times for the member consult"
    );
}

/// E5-R R3: the re-park RENEWS the document's retention stamp from the
/// publication timestamp plus the configured `park_ttl` (7200s here) — the
/// earliest outstanding ticket no longer derives it. The E7 test-7 drive
/// shape by ADDITION ([`reparked_blocking_entries_carry_the_new_calls_own_deadline`]
/// is untouched): a `world_over_hitl` variant whose
/// `ParkConfig.park_ttl = 7200` rides the same 207-poll route
/// (`timeout_secs` 3600), and the mid-segment re-park raises two fresh
/// gated calls — the sibling parks first with the EARLIER ticket, the
/// target second — both undecided in the store at the commit. The ttl
/// differs from the ticket windows, so the interim bridge's value and the
/// E5 value are observably different instants.
///
/// RED today: the interim bridge renews the stamp from the earliest
/// outstanding ticket — the sibling's ≈ +1h deadline — so both stamp
/// assertions fail at the value.
///
/// Guard (green today and after E5): the TARGET blocking entry still equals
/// the target ticket's OWN deadline — E7's per-call deadline semantics are
/// untouched by the stamp.
#[tokio::test]
async fn reparked_document_renews_retention_from_the_publication_timestamp() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let (url, receiver) = park_receiver();
    let mut world = world_over_hitl(|registry| {
        let config = aura_config::HitlConfig {
            require_approval: vec![aura_config::GlobPattern::new("kubectl_*").unwrap()],
            park: aura_config::ParkConfig {
                enabled: true,
                bind_identity: false,
                park_ttl: aura_config::ParkTtl::try_new(7200).expect("park ttl validates"),
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

    // The re-park pair's rig ids and provider call ids, distinct per frame:
    // the loop keys tool results by the rig id and the park stamps each
    // pending call's id from the one it observed.
    const SIBLING_RIG_ID: &str = "call_r0";
    const TARGET_RIG_ID: &str = "call_r1";
    const SIBLING_CALL_ID: &str = "call_id_r0";
    const TARGET_CALL_ID: &str = "call_id_r1";

    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let sibling_invocations = Arc::new(Mutex::new(Vec::new()));
    let target_invocations = Arc::new(Mutex::new(Vec::new()));
    // ONE assistant turn issues BOTH gated calls: the sibling parks first
    // (its gate entry mints the EARLIER ticket deadline), the target second
    // (the LATER one). The stream ends after the one batch — the park hook
    // cancels after the snapshot, before the next completion.
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
            ScriptedToolCall::new(SIBLING_RIG_ID, TOOL_B, json!({ "namespace": "stage" }))
                .with_call_id(SIBLING_CALL_ID),
            ScriptedToolCall::new(TARGET_RIG_ID, NEW_TOOL, json!({ "namespace": "stage" }))
                .with_call_id(TARGET_CALL_ID),
        ])]),
        extra_tools: vec![
            Box::new(RecordingTool::new(apply_invocations.clone()).with_name(TOOL)),
            Box::new(RecordingTool::new(sibling_invocations.clone()).with_name(TOOL_B)),
            Box::new(RecordingTool::new(target_invocations.clone()).with_name(NEW_TOOL)),
        ],
    }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the decided checkpoint grants: the mid-segment park has not run yet");
    let republish_before = chrono::Utc::now();
    let segment = run_segment_live(grant, &world.config, &HashMap::new())
        .await
        .expect("the segment re-parks on the two fresh gated calls");
    let republish_after = chrono::Utc::now();

    assert_eq!(
        apply_invocations
            .lock()
            .expect("apply invocation log")
            .len(),
        1,
        "the decided call executes exactly once before the re-park"
    );
    assert!(
        sibling_invocations
            .lock()
            .expect("sibling invocation log")
            .is_empty()
            && target_invocations
                .lock()
                .expect("target invocation log")
                .is_empty(),
        "neither freshly parked call executed its tool"
    );
    assert!(
        matches!(segment, ResumeStreamEnd::Reparked),
        "the segment re-parks on the two fresh gated calls: {segment:?}"
    );
    // Both fresh tickets live in the store at the commit — undecided, each
    // inside its own window — and the sibling's is the earlier deadline.
    let pending = world
        .store
        .list_pending()
        .await
        .expect("the store lists its undecided approvals");
    assert_eq!(
        pending.len(),
        2,
        "the re-park registered both freshly gated calls"
    );
    let sibling_ticket = pending
        .iter()
        .find(|ticket| ticket.request.items[0].tool_name == TOOL_B)
        .expect("the sibling ticket names the sibling call");
    let target_ticket = pending
        .iter()
        .find(|ticket| ticket.request.items[0].tool_name == NEW_TOOL)
        .expect("the target ticket names the newly gated call");
    assert!(
        sibling_ticket.expires_at > chrono::Utc::now(),
        "the sibling's fresh ticket sits inside its own window"
    );
    assert!(
        sibling_ticket.expires_at < target_ticket.expires_at,
        "the fixture shape: the sibling parks first and holds the EARLIER \
         ticket deadline; the target's is the later one"
    );

    let republished = load_parked_run(&parked_document_path(&world))
        .await
        .expect("the re-park re-published the checkpoint under the parked name");
    let renewed = republished.retention_expires_at.as_datetime();

    let lower = republish_before + chrono::Duration::seconds(7200 - 30);
    let upper = republish_after + chrono::Duration::seconds(7200 + 30);
    assert!(
        renewed >= lower && renewed <= upper,
        "the renewed retention stamp is the re-publication timestamp plus the \
         park_ttl (7200s): got {renewed}, expected within [{lower}, {upper}]"
    );
    assert!(
        (renewed - sibling_ticket.expires_at).num_seconds().abs() >= 60,
        "the renewed stamp must not derive from the earliest outstanding \
         ticket: {renewed} sits within one minute of the sibling ticket's \
         deadline {}",
        sibling_ticket.expires_at
    );
}

/// Regression guard (green today): the expired arms unlink the parked
/// checkpoint, sweep the undecided approvals, tolerate a retried resume of
/// the already-unlinked run (the NotFound-tolerant unlink keeps the arms
/// idempotent), and the retry answers the absent row.
#[tokio::test]
async fn expired_teardown_unlinks_sweeps_and_stays_idempotent() {
    let world = world();
    register_undecided(&world).await;
    // One undecided member past its document's retention stamp.
    publish_document(
        &world,
        &sentinel_document_with_retention(&world, PAST_STAMP),
    )
    .await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err("the retention-expired run refuses");
    match &refusal {
        ResumeRefusal::Conflict(row) => {
            let body = serde_json::to_value(row).expect("the conflict row serializes");
            assert_eq!(body["code"], "expired", "the terminal expired code: {body}");
            assert_eq!(
                body["detail"], "the decision window closed before every pending call was decided",
                "the expired detail: {body}"
            );
        }
        other => panic!("expected the expired row, got {other:?}"),
    }
    assert!(
        !parked_document_path(&world).exists() && !resuming_document_path(&world).exists(),
        "the expired teardown unlinked the parked checkpoint"
    );
    assert!(
        world
            .registry
            .try_parked(&decision())
            .await
            .expect("the store reads")
            .is_none(),
        "the expired teardown swept the undecided approval"
    );
    let retry = evaluate_resume(evaluation(&world, false, None)).await;
    assert!(
        matches!(retry, Err(ResumeRefusal::DocumentAbsent)),
        "a retried resume of the same run answers the absent row: {retry:?}"
    );
}

// =====================================================================
// S5-RED frames (the fresh-budget wave's RED, aura/P45 #271 S5).
// Dispatch contract "Resume SSE input": the resumed worker gets the
// configured per-call timeout, maximum depth, and fresh call counters;
// the resumed coordinator gets the configured outer server timeout
// through the normal chain and a fresh local cycle counter (the
// three-fresh-cycle ceiling is S4 frame 11's golden — not re-derived
// here). Each frame stages a fixture whose fresh-budget inputs are
// CONFIGURED values distinct from every default the fixtures otherwise
// carry (the retention wave's distinct-value discipline), then pins the
// resumed execution honoring exactly those values.
//
// Verification-unit arrival states (brief line 22): each frame's
// doc-comment names its arrival state honestly — red-with-reason where
// the wiring is absent at this tip, green-guard where the S3/S4 fills
// already established the behavior (a green guard here is a
// contract-proof guard, never a manufactured failure).
// =====================================================================

/// How long a borrowed-driver probe waits before declaring the driver
/// still blocked on its in-flight tail or its drain — the same bound the
/// S4-R frames' `BORROWED_PROBE` holds. Frame 1's honest-RED bound rides
/// the same constant: the unfilled wiring never ends, the bound declares
/// it in bounded time, and the suite never hangs.
const S5_STALL_BOUND: Duration = Duration::from_secs(5);

/// The world with caller-supplied per-call timeout (seconds): the default
/// webhook-poll world behind a config whose `per_call_timeout_secs` stages
/// the fresh budget a worker-budget frame pins. The mutation happens
/// before any fixture is built, so the published checkpoint matches the
/// timeout-bearing config's fingerprint.
fn world_with_per_call_timeout(secs: u64) -> World {
    let mut world = world();
    if let Some(mut orchestration) = world.config.orchestration.take() {
        orchestration.timeouts.per_call_timeout_secs = secs;
        world.config.orchestration = Some(orchestration);
    }
    world
}

/// The world with the `operations` worker's configured turn depth: the
/// default webhook-poll world behind a config whose worker turn depth
/// stages the fresh ceiling a depth frame pins. Mutated before any
/// fixture build, so the checkpoint matches the config's fingerprint.
fn world_with_turn_depth(depth: usize) -> World {
    let mut world = world();
    if let Some(mut orchestration) = world.config.orchestration.take() {
        if let Some(worker) = orchestration.workers.get_mut("operations") {
            worker.turn_depth = Some(depth);
        }
        world.config.orchestration = Some(orchestration);
    }
    world
}

/// A worker override whose scripted continuation issues the probe tool
/// calls exactly as scripted: each call a distinct (id, arguments) pair so
/// the duplicate-call guard's fingerprint never pairs them, riding the
/// ungated `deploy_probe` name so the drive loop never touches the park
/// machinery between turns.
fn s5_probe_call(rig_id: &str, args: Value) -> ScriptedToolCall {
    ScriptedToolCall::new(rig_id, PROBE_TOOL, args)
}

/// Frame 1 (S5) — RED today (wiring absent). A parked run whose config
/// sets per-call timeout 1s resumes with that exact deadline bounding its
/// continuation's provider call. The fixture value is distinct from every
/// default (the config default is 120s; the fixture is 1s), so an
/// implementation that keeps some other deadline cannot pass.
///
/// Vehicle: after the decided call executes, the resumed continuation's
/// FIRST provider request is stalled at the MODEL level — the scripted
/// model records the request, then holds on a stall hook, the
/// deterministic stand-in for a provider that stops answering
/// mid-request. The configured per-call budget must kill that stream at
/// 1s and the segment must fault with an error naming the configured
/// one-second deadline.
///
/// Scope ruling (owner, recorded on P45): the per-call timeout bounds the
/// PROVIDER CALL, matching the chat path's `stream_and_forward` wrap — the
/// same authority the dispatch contract names ("the same fields the normal
/// chat path reads"). Mid-tool-execution interruption is OUT of contract
/// by design: the resumed run's tool invocations are tracked on the run's
/// execution scope precisely so a tail outlives the segment
/// (resume/DESIGN.md:336-348, streaming_request_hook.rs:367 — cancellation
/// happens between operations, not mid-tool execution), and the S4 error
/// arm's drain waits those tracked tasks out. A stalled TOOL is therefore
/// not this frame's vehicle; a stalled PROVIDER RESPONSE is.
///
/// RED today with reason: the resumed worker's continuation stream is
/// started with a hard-coded `Duration::MAX` hook timeout and no
/// `per_call_timeout_secs` wrap (orchestrator.rs, the segment's
/// `stream_chat_message_with_timeout` call), so the configured deadline
/// never bounds it — the stalled provider request runs past the frame's
/// bound and the outer timeout declares it. The bound keeps the RED
/// bounded: the suite reads a deterministic failure, never a hang. After
/// the fill the continuation faults within 1s and the pinned deadline
/// text appears.
#[tokio::test]
async fn s5_resumed_worker_uses_the_configured_per_call_timeout() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let stall = StallHook::new();
    let world = world_with_per_call_timeout(1);
    let model = ScriptedCompletionModel::new(vec![ScriptedTurn::text(
        "the resumed worker adapts and reports",
    )])
    .with_stall(stall.clone());
    let requests = model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model,
        extra_tools: vec![Box::new(
            RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL),
        )],
    }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(64);
    let (_entered, end) = tokio::join!(
        async {
            tokio::time::timeout(S5_STALL_BOUND, stall.wait_entered())
                .await
                .expect("the stalled provider request engaged within the bound")
        },
        async {
            tokio::time::timeout(
                S5_STALL_BOUND,
                run_segment_borrowed(
                    &grant,
                    &world.config,
                    &HashMap::new(),
                    event_tx,
                    crate::UsageState::new(),
                    None,
                ),
            )
            .await
        },
    );
    let end = match end {
        Ok(result) => result,
        Err(_elapsed) => panic!(
            "RED with reason: the configured per-call timeout (1s) never bounds \
             the resumed worker's continuation stream — the segment starts it
             with a hard-coded `Duration::MAX` hook timeout and no
             `per_call_timeout_secs` wrap, so a stalled provider request
             cannot be interrupted (the bound was reached)"
        ),
    };
    let Err(fault) = end else {
        panic!(
            "the stalled continuation must fault under the configured per-call \
             timeout — the segment instead finished or parked, which no honest
             fresh per-call budget can produce"
        )
    };
    let SegmentError::Continuation(diagnostic) = &fault;
    let text = diagnostic.to_string();
    assert!(
        text.contains("timed out after 1s"),
        "the continuation stream must carry the configured per-call deadline \
         (1s) in its error — the same config field the normal chat path \
         reads, not some resumed default: {text}"
    );
    assert_eq!(
        requests.lock().expect("scripted request log").len(),
        1,
        "the deadline killed the in-flight provider request — the request \
         the stalled model recorded is the bounded one"
    );
}

/// Frame 2 (S5) — arrival state observed live: a nested continuation past
/// the configured maximum depth stops at the configured ceiling, not the
/// checkpoint's historical anything. The fixture configures the
/// `operations` worker's turn depth at 2 (the rig default is 16 — the
/// interim and target must not coincide) and deliberately carries a deep
/// checkpoint history (ten historical turns) so no historically-derived
/// depth can masquerade as the configured ceiling.
///
/// The continuation's script issues MORE probe turns than the ceiling:
/// fresh requests must stop at the configured depth, the ceiling breach
/// asserts itself, and the segment ends on that error naming the
/// configured limit of 2 — never a deeper budget, never a
/// checkpoint-derived one.
#[tokio::test]
async fn s5_resumed_worker_applies_the_configured_maximum_depth() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world_with_turn_depth(2);
    let probe_invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        // Four probe turns — one turn more than the rig ceiling check can
        // ever grant at depth 2, distinct arguments each so the guard's
        // fingerprint arithmetic stays out of the pins. The final text
        // turn is never requested under the ceiling.
        model: ScriptedCompletionModel::new(vec![
            ScriptedTurn::tool_calls(vec![s5_probe_call(
                "call_depth_0",
                json!({ "step": "one" }),
            )]),
            ScriptedTurn::tool_calls(vec![s5_probe_call(
                "call_depth_1",
                json!({ "step": "two" }),
            )]),
            ScriptedTurn::tool_calls(vec![s5_probe_call(
                "call_depth_2",
                json!({ "step": "three" }),
            )]),
            ScriptedTurn::tool_calls(vec![s5_probe_call(
                "call_depth_3",
                json!({ "step": "four" }),
            )]),
            // The fifth turn exists ONLY to prove the stop came from the
            // ceiling: an extended or checkpoint-derived budget or an
            // unguarded loop would consume it; the configured ceiling 2
            // must never reach it.
            ScriptedTurn::tool_calls(vec![s5_probe_call(
                "call_depth_4",
                json!({ "step": "five" }),
            )]),
        ]),
        extra_tools: vec![
            Box::new(RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL)),
            Box::new(RecordingTool::new(probe_invocations.clone()).with_name(PROBE_TOOL)),
        ],
    }]);
    register_decided(&world).await;
    // The fixture adds the historical TURN depth: ten historical turns on
    // the awaiting node — the conversation the checkpoint records as
    // evidence. A fresh budget derived from the checkpoint's depth instead
    // of the configured 2 would fail this frame from the fixture alone.
    let mut document = sentinel_document(&world);
    if let Some(history) = document.plan.tasks.first_mut().unwrap().history.as_mut() {
        for i in 0..10 {
            history.push(rig::completion::Message::user(format!(
                "historical turn {i}"
            )));
            history.push(rig::completion::Message::assistant(
                "historical settled step",
            ));
        }
    }
    publish_document(&world, &document).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants despite the deep checkpoint history");

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(64);
    let end = run_segment_borrowed(
        &grant,
        &world.config,
        &HashMap::new(),
        event_tx,
        crate::UsageState::new(),
        None,
    )
    .await;
    let Err(fault) = end else {
        panic!(
            "a continuation past the configured ceiling must stop at it — the \
             segment instead finished or re-parked, which no honest fresh \
             ceiling can produce"
        )
    };
    let SegmentError::Continuation(diagnostic) = &fault;
    let text = diagnostic.to_string();
    assert!(
        text.contains("reached limit: 2"),
        "the continuation stops at the CONFIGURED ceiling (turn depth 2) — \
          not the checkpoint's historical depth and not any default: {text}"
    );
    assert!(
        !text.contains("script exhausted"),
        "the stop reason is the ceiling breach itself, never an exhausted \
         script read-through: {text}"
    );
    let recorded = probe_invocations
        .lock()
        .expect("probe invocation log")
        .len();
    assert_eq!(
        recorded, 4,
        "the loop stops at the configured ceiling's turn boundary — the fifth \
         scripted turn is never executed, and neither a script exhaustion nor \
         any checkpoint-derived budget produced the stop (executed {recorded})"
    );
}

/// Frame 3 (S5) — green-guard. The resumed worker's call counters (the
/// duplicate-call guard's escalation counts, seeded from the configured
/// nudge/block thresholds) start FRESH for the resumed execution: the
/// checkpoint's historical counts seed nothing. The fixture configures
/// nudge=1 / block=2 (the rig defaults are 3 / 5 — the interim and target
/// must not coincide) and stages two pre-park executions of the very call
/// the continuation repeats — recorded in the node's conversation history
/// as the evidence a park document carries (a grantable checkpoint's
/// executed-tombstone ledger always reads empty; the consult refuses a
/// non-empty one as death evidence) — so a counter seeded from history
/// would open the fresh stream already past the configured nudge, and
/// even one seeded prior call would point the very first fresh call at
/// `DUPLICATE_CALL_ABORT` instead of the configured nudge.
///
/// The fresh stream must therefore run the configured arithmetic from
/// zero: the first fresh repeat annotates with the configured nudge
/// (`[DUPLICATE_CALL_GUIDANCE]`), the second with the configured block
/// (`[DUPLICATE_CALL_ABORT]`) — the checkpoint's two historical executions
/// present and counted as nothing.
#[tokio::test]
async fn s5_resumed_worker_call_counters_start_fresh_from_the_configuration() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world_with_work_call_thresholds(1, 2);
    let probe_invocations = Arc::new(Mutex::new(Vec::new()));
    // Node A's script uses the SAME probe arguments in both fresh turns —
    // the exact invocation pattern whose historical executions sit in the
    // checkpoint — so the fresh-counter zero-start is proven against the
    // strongest witness: even one seeded prior call would point the very
    // first fresh call at `DUPLICATE_CALL_ABORT` instead of the
    // configured nudge. The worker build consumes the queued override;
    // this handle holds a clone sharing the same request log the build's
    // model drives, so the request-log pins read the real turn requests.
    let counter_model = ScriptedCompletionModel::new(vec![
        ScriptedTurn::tool_calls(vec![s5_probe_call(
            "call_fresh_0",
            json!({ "namespace": "s5-counts" }),
        )])
        .with_text("fresh repeat one"),
        ScriptedTurn::tool_calls(vec![s5_probe_call(
            "call_fresh_1",
            json!({ "namespace": "s5-counts" }),
        )])
        .with_text("fresh repeat two"),
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_sub_counters",
            "submit_result",
            json!({
                "summary": "fresh counters stage pinned",
                "result": "the fresh calls escalate on their own counts",
                "confidence": "high",
            }),
        )]),
    ]);
    let counter_requests = counter_model.requests();
    install_worker_overrides(vec![WorkerOverride {
        model: counter_model,
        extra_tools: vec![
            Box::new(RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL)),
            Box::new(RecordingTool::new(probe_invocations.clone()).with_name(PROBE_TOOL)),
        ],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
    register_decided(&world).await;
    // The checkpoint carries the historical executions as EVIDENCE: two
    // pre-park executions of the very call the fresh turns repeat, their
    // returns recorded in the node's conversation history — the shape a
    // live park captures. A counter seeded from that history fails the
    // frame; fresh counters read it as nothing.
    let mut document = sentinel_document(&world);
    {
        let node = document.plan.tasks.first_mut().expect("the awaiting node");
        let history = node.history.as_mut().expect("the node's history");
        history.push(tool_call_turn(vec![assistant_tool_call(
            "call_hist_p0",
            PROBE_TOOL,
            &json!({ "namespace": "s5-counts" }),
        )]));
        history.push(tool_result_prompt(
            "call_hist_p0",
            &tool_wire(ECHO_TOOL_RESULT),
        ));
        history.push(tool_call_turn(vec![assistant_tool_call(
            "call_hist_p1",
            PROBE_TOOL,
            &json!({ "namespace": "s5-counts" }),
        )]));
        history.push(tool_result_prompt(
            "call_hist_p1",
            &tool_wire(ECHO_TOOL_RESULT),
        ));
    }
    publish_document(&world, &document).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(64);
    let end = run_segment_borrowed(
        &grant,
        &world.config,
        &HashMap::new(),
        event_tx,
        crate::UsageState::new(),
        None,
    )
    .await
    .expect("the segment driver did not panic");
    let ResumeStreamEnd::Completed { final_answer } = &end else {
        panic!("the fresh counters stage completes: {end:?}")
    };
    assert_eq!(
        final_answer, COORD_FINAL_ANSWER,
        "the staged run finishes naturally on the fresh counters"
    );
    assert_eq!(
        probe_invocations
            .lock()
            .expect("probe invocation log")
            .len(),
        2,
        "exactly the fresh stream's two repeats execute — the checkpoint's \
         historical executions are evidence, never re-runs"
    );

    // The fresh requests' histories carry the configured escalation:
    // the SECOND turn sees the first fresh repeat's result annotated with
    // the configured GUIDANCE (fresh count 1 reached nudge 1), and the
    // THIRD turn sees the second fresh repeat's result annotated with the
    // configured ABORT (fresh count 2 reached block 2). A counter seeded
    // from the checkpoint's two historical executions would have opened
    // the fresh stream already aborting — the distinct-value witness the
    // fixture stages on purpose.
    let logged = counter_requests.lock().expect("worker request log").clone();
    assert_eq!(
        logged.len(),
        3,
        "exactly the scripted three fresh turns are requested: {}",
        logged.len()
    );
    let second_saw = format!("{:?}", logged[1]).contains(ECHO_TOOL_RESULT);
    if second_saw {
        // Turn 2's history carries turn 1's fresh probe result: annotated
        // with the configured nudge, never the block.
        let body = format!("{:?}", logged[1]);
        assert!(
            body.contains("[DUPLICATE_CALL_GUIDANCE]"),
            "the first FRESH repeat escalates on the configured nudge (1), \
             not on a history-seeded count: {body}"
        );
        assert!(
            !body.contains("[DUPLICATE_CALL_ABORT]"),
            "the first FRESH repeat must not reach the configured block (2): \
             a seeded counter would have: {body}"
        );
        let body = format!("{:?}", logged[2]);
        assert!(
            body.contains("[DUPLICATE_CALL_ABORT]"),
            "the second FRESH repeat escalates on the configured block (2): \
             the fresh counters derive from the configuration alone: {body}"
        );
    } else {
        panic!(
            "turn 2's request must carry turn 1's fresh probe result — the \
             scripted result text (\"{}\") is missing from the request log",
            ECHO_TOOL_RESULT
        );
    }
}

/// Frame 4 (S5) — green-guard (the S3 projection at the factory seam is
/// already golden: `s3_outer_budget_projects_from_the_factory_timeout_...`
/// in factory.rs pins the projection seam; this frame pins the RECEIPT).
/// The outer server timeout reaches the resumed coordinator through the
/// same chain the normal path uses — factory timeout → `outer_budget` →
/// the resumed coordinator's budget check — not a resume-specific bypass.
///
/// Arm A: a nonzero factory timeout whose whole budget (2s) cannot fit
/// even one configured per-call slice (7s) — the resumed coordinator's
/// first post-execute `create_plan` decision hits the normal
/// 'Time budget exhausted' arm and the loop stops on ONE scripted
/// decision turn, its final answer carrying that stopping reason.
///
/// Arm B: the same fixture under a timeout large enough to fund its
/// slices (600s) replans normally — three exact decision turns consumed,
/// the fourth never requested (the fresh cycle budget S4 frame 11 pins
/// does that stopping; the outer chain does not). The only input
/// difference between the arms is the factory timeout argument, so the
/// budget the coordinator reads is the projected outer value, through the
/// normal chain.
#[tokio::test]
async fn s5_resumed_coordinator_receives_the_outer_server_timeout_through_the_normal_chain() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    // Arm B installs FOUR worker overrides (the restored node plus one per
    // permitted fresh cycle); the fourth is never requested. Arm A
    // installs one. The drain guard keeps either arm's leftovers off the
    // process-global queue.
    let _drain = OverrideDrain;

    fn budget_world() -> World {
        world_with_per_call_timeout(7)
    }

    // Arm A: budget 2s < one per-call slice 7s → the chain engages.
    let arm_a_world = budget_world();
    register_decided(&arm_a_world).await;
    publish_document(&arm_a_world, &sentinel_document(&arm_a_world)).await;
    let arm_a_grant = evaluate_resume(evaluation(&arm_a_world, false, None))
        .await
        .expect("the all-decided run grants");
    let arm_a_coordinator = ScriptedCompletionModel::new(vec![planning_turn_for("cycle one")]);
    let arm_a_requests = arm_a_coordinator.requests();
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: arm_a_coordinator,
    }]);
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![
            ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                "call_sub_s5a",
                "submit_result",
                json!({
                    "summary": "arm a settled",
                    "result": "the restored node completed under arm a",
                    "confidence": "high",
                }),
            )])
            .with_text(A_DONE),
        ]),
        extra_tools: vec![Box::new(
            RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL),
        )],
    }]);
    let arm_a_factory = OrchestratorFactory::new(arm_a_world.config.clone())
        .with_reservation_table(Arc::new(ResumeClaimTable::new()));

    let (mut arm_a_stream, _arm_a_cancel, _arm_a_usage) = arm_a_factory
        .resume_stream_with_timeout(arm_a_grant, Duration::from_secs(2), "req_s5_chain_some")
        .await;
    let arm_a_text = s5_driven_final_text(&mut arm_a_stream).await;
    assert_eq!(
        arm_a_requests
            .lock()
            .expect("coordinator request log")
            .len(),
        1,
        "the projected Some(timeout) outer budget stops the resumed coordinator \
         loop at its FIRST create_plan decision ('Time budget exhausted') — the \
         factory timeout reached the segment through the normal chain"
    );
    assert!(
        arm_a_text.contains("Time budget exhausted"),
        "the projected budget's exhausted arm rides the resumed run's final \
         answer: {arm_a_text:?}"
    );

    // Arm B: the same fixture shape under a timeout large enough to fit
    // every configured slice — the chain must NOT disturb the loop, whose
    // own cycle budget is the only stopper.
    let arm_b_world = budget_world();
    register_decided(&arm_b_world).await;
    publish_document(&arm_b_world, &sentinel_document(&arm_b_world)).await;
    let arm_b_grant = evaluate_resume(evaluation(&arm_b_world, false, None))
        .await
        .expect("the all-decided run grants");
    let arm_b_coordinator = ScriptedCompletionModel::new(vec![
        planning_turn_for("cycle one"),
        planning_turn_for("cycle two"),
        planning_turn_for("cycle three"),
        planning_turn_for("the fourth fresh cycle must never be requested"),
    ]);
    let arm_b_requests = arm_b_coordinator.requests();
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: arm_b_coordinator,
    }]);
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(A_DONE)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL),
            )],
        },
        s5_cycle_worker_override("arm b fresh cycle one"),
        s5_cycle_worker_override("arm b fresh cycle two"),
        s5_cycle_worker_override("arm b fresh cycle three"),
    ]);
    let arm_b_factory = OrchestratorFactory::new(arm_b_world.config.clone())
        .with_reservation_table(Arc::new(ResumeClaimTable::new()));

    let (mut arm_b_stream, _cancel, _usage) = arm_b_factory
        .resume_stream_with_timeout(arm_b_grant, Duration::from_secs(600), "req_s5_chain_none")
        .await;
    let arm_b_text = s5_driven_final_text(&mut arm_b_stream).await;
    assert_eq!(
        arm_b_requests
            .lock()
            .expect("coordinator request log")
            .len(),
        3,
        "the projector's None-unbounded shape — a budget that fits its slices \
         — never disturbs the resumed coordinator loop: three decision turns \
         consumed, the fourth never requested"
    );
    assert!(
        arm_b_text.contains("Replan budget exhausted"),
        "the wide-budget arm stops on the cycle budget alone, never the outer \
         chain: {arm_b_text:?}"
    );
}

/// One worker override for a fresh-planned `operations` task: a single
/// submit-result turn whose marker text is the cycle's own.
fn s5_cycle_worker_override(marker: &str) -> WorkerOverride {
    WorkerOverride {
        model: ScriptedCompletionModel::new(vec![
            ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                "call_sub_cycle",
                "submit_result",
                json!({
                    "summary": marker,
                    "result": marker,
                    "confidence": "high",
                }),
            )])
            .with_text(marker),
        ]),
        extra_tools: vec![],
    }
}

/// Drive a resume stream to its `Final` terminal, returning the content
/// that terminal carries — the frame-4 arms' final-answer probe.
async fn s5_driven_final_text<S>(stream: &mut S) -> String
where
    S: futures::Stream<Item = Result<StreamItem, StreamError>> + Unpin,
{
    use futures::StreamExt;
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let Ok(StreamItem::Final(info)) = item {
            text = info.content;
            break;
        }
    }
    text
}

/// Frame 5 (S5) — green-guard. Historical turn/call numbering on the
/// restored checkpoint is preserved as evidence and never constrains the
/// fresh limits: a checkpoint whose historical depth sits near the max —
/// iteration 8, an executed-tombstone ledger of six historical handles,
/// and a ten-turn historical worker history — still gets the FULL
/// configured fresh depth (turn depth 3 here; the rig default is 16, so
/// the interim and a checkpoint-derived budget must not coincide).
///
/// The continuation spends the full fresh budget — two probe calls and
/// the natural submit — stops nowhere, requests the fourth turn never,
/// and completes with the natural finish; the historical numbering rides
/// the checkpoint untouched as the evidence it is (a grantable
/// checkpoint's executed-tombstone ledger always reads empty, so the
/// historical executions stage in the conversation history instead).
#[tokio::test]
async fn s5_historical_checkpoint_numbering_stays_separate_from_fresh_limits() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world_with_turn_depth(3);
    let probe_invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![
            ScriptedTurn::tool_calls(vec![s5_probe_call("call_full_0", json!({ "step": "one" }))])
                .with_text("full budget step one"),
            ScriptedTurn::tool_calls(vec![s5_probe_call("call_full_1", json!({ "step": "two" }))])
                .with_text("full budget step two"),
            ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                "call_sub_full",
                "submit_result",
                json!({
                    "summary": "full fresh depth spent",
                    "result": "the continuation reached its natural finish",
                    "confidence": "high",
                }),
            )]),
            // The fourth scripted turn exists ONLY to prove the fresh
            // budget was spent exactly: an extended or checkpoint-derived
            // budget might reach it; the configured one must not.
            ScriptedTurn::tool_calls(vec![s5_probe_call(
                "call_full_3",
                json!({ "step": "four" }),
            )]),
        ]),
        extra_tools: vec![
            Box::new(RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL)),
            Box::new(RecordingTool::new(probe_invocations.clone()).with_name(PROBE_TOOL)),
        ],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
    register_decided(&world).await;
    // The fixture's historical numbering, near the max everywhere:
    // iteration 8, six executed tombstones, and ten historical turns on
    // the awaiting node's history with their own numbered call ids.
    let mut document = sentinel_document(&world);
    document.iteration = 8;
    if let Some(history) = document.plan.tasks.first_mut().unwrap().history.as_mut() {
        for i in 0..10 {
            history.push(rig::completion::Message::user(format!(
                "historical turn {i}"
            )));
            history.push(rig::completion::Message::assistant(
                "historical settled step",
            ));
        }
    }
    publish_document(&world, &document).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants despite the near-max historical numbering");

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(64);
    let end = run_segment_borrowed(
        &grant,
        &world.config,
        &HashMap::new(),
        event_tx,
        crate::UsageState::new(),
        None,
    )
    .await
    .expect("the segment driver did not panic");
    let ResumeStreamEnd::Completed { final_answer } = &end else {
        panic!("the fresh limits complete the near-max historical checkpoint: {end:?}")
    };
    assert_eq!(
        final_answer, COORD_FINAL_ANSWER,
        "the historical numbering neither shortened nor obstructed the fresh \
         run's natural finish"
    );
    assert_eq!(
        probe_invocations
            .lock()
            .expect("probe invocation log")
            .len(),
        2,
        "the FULL fresh budget's probe calls executed"
    );
}

// =====================================================================
// S4-R frames (the SSE wave's RED, aura/P45 joint hole #12): the
// borrowed-grant driver. Each frame drives one whole resume segment
// through `run_segment_borrowed` — the same fixtures the atomic
// segment goldens stage, now observed at the streaming boundary —
// RED today at the hole's named todo.
// =====================================================================

use crate::orchestration::OrchestratorEvent;
use crate::orchestration::OrchestratorFactory;
use crate::provider_agent::{FinalResponseInfo, StreamError, StreamItem};
use futures::StreamExt;

/// A stencil coordinator turn whose decision is `create_plan` over the
/// one fresh task — the repeated-cycle shape the fresh-cycle-counter
/// frame scripts four of.
fn planning_turn_for(marker: &str) -> ScriptedTurn {
    ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
        "call_plan",
        "create_plan",
        json!({
            "goal": marker,
            "steps": [
                {"type": "task", "task": marker, "worker": "operations"}
            ],
            "routing_rationale": "the restored run has tool work to finish",
            "planning_summary": marker,
        }),
    )])
    .with_text(marker)
}

/// The world with caller-supplied duplicate-call thresholds: the default
/// webhook-poll world behind a config whose guard thresholds stage the
/// fresh-counter arithmetic the call-counter frame pins. The mutation
/// happens before the frame computes the fingerprint, so the published
/// checkpoint matches the threshold-bearing config.
fn world_with_work_call_thresholds(nudge: usize, block: usize) -> World {
    let mut world = world();
    if let Some(mut orchestration) = world.config.orchestration.take() {
        orchestration.duplicate_call_nudge_threshold = nudge;
        orchestration.duplicate_call_block_threshold = block;
        world.config.orchestration = Some(orchestration);
    }
    world
}

/// How long a borrowed-driver probe waits before declaring the driver
/// still blocked on its in-flight tail or its drain — the same bound the
/// atomic golden's `DRAIN_PROBE` holds (long enough for a segment body
/// that does NOT block, short enough that a regression reads as
/// still-blocked instead of hanging the suite).
const BORROWED_PROBE: Duration = Duration::from_secs(5);

/// Count the `Final` terminals a borrowed stream aggregates, driving it
/// to its end: the terminal-count probe the completed-arm pins key on.
async fn count_final_terminals<S>(stream: &mut S) -> usize
where
    S: futures::Stream<Item = Result<StreamItem, StreamError>> + Unpin,
{
    use futures::StreamExt;
    let mut count = 0;
    while let Some(item) = stream.next().await {
        if let Ok(StreamItem::Final(_)) = item {
            count += 1;
        }
    }
    count
}

/// Collect every item a borrowed stream forwards, driving it to its end.
async fn drive_to_items<S>(stream: &mut S) -> Vec<Result<StreamItem, StreamError>>
where
    S: futures::Stream<Item = Result<StreamItem, StreamError>> + Unpin,
{
    use futures::StreamExt;
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

/// The hand-summarized `RunParked` events the forwarded items carry, in
/// wire order — the exact-once probe the publication-owner frame keys on.
/// (`OrchestratorEvent` is Debug-only, so the event rides as prose.)
fn run_parked_events(items: &[Result<StreamItem, StreamError>]) -> Vec<String> {
    items
        .iter()
        .filter_map(|result| match result {
            Ok(StreamItem::OrchestratorEvent(OrchestratorEvent::RunParked {
                run_id,
                decision_ids,
                retention_expires_at: _,
                iteration,
            })) => Some(format!(
                "RunParked(run_id={run_id}, decision_ids={decision_ids:?}, iteration={iteration})"
            )),
            _ => None,
        })
        .collect()
}

/// Whether none of the forwarded items is a `Final` terminal — the
/// no-duplicate-final probe the re-parked arm pins.
fn carries_no_final(items: &[Result<StreamItem, StreamError>]) -> bool {
    items
        .iter()
        .all(|result| !matches!(result, Ok(StreamItem::Final(_))))
}

/// Frame 8 (S4): a segment completing mid-run yields
/// `ResumeStreamEnd::Completed { final_answer }` and the stream it
/// streams over carries EXACTLY ONE terminal — the borrowed driver
/// emits no final of its own, so the normal factory finalization's
/// single emission is the only one the client sees.
#[tokio::test]
async fn s4_completed_segment_hands_final_answer_to_normal_finalization_no_duplicate_final() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let invocations = Arc::new(Mutex::new(Vec::new()));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(RecordingTool::new(invocations).with_name(TOOL))],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");

    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(64);
    // The factory's own finalization would send its one terminal over
    // this same channel: clone the sender BEFORE the driver consumes
    // the original, so the frame's stream carries it as queued work.
    let finalization_tx = event_tx.clone();
    let finalized_marker: FinalResponseInfo = FinalResponseInfo {
        content: "the normal factory finalization emitted this single terminal".to_string(),
        usage: Default::default(),
        cache_usage: None,
    };

    let mut stream = Box::pin(futures::stream::unfold(event_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));

    let end = tokio::time::timeout(
        BORROWED_PROBE,
        run_segment_borrowed(
            &grant,
            &world.config,
            &HashMap::new(),
            event_tx,
            crate::UsageState::new(),
            None,
        ),
    )
    .await
    .expect("the borrowed segment returns within the bound")
    .expect("the segment driver did not panic");

    let ResumeStreamEnd::Completed { final_answer } = &end else {
        panic!("a completed segment yields Completed, got {end:?}")
    };
    assert_eq!(
        final_answer, COORD_FINAL_ANSWER,
        "the returned final answer is the run's natural finish, not the driver's own"
    );

    // The driver forwarded no terminal of its own: the completion leg's
    // synthetic Final is queued first, and it is the ONLY one to arrive
    // once the driver exits (dropping the original sender).
    finalization_tx
        .send(Ok(StreamItem::Final(finalized_marker)))
        .await
        .expect("the channel is open for the normal finalization leg");
    drop(finalization_tx);

    let count = count_final_terminals(&mut stream).await;
    assert_eq!(
        count, 1,
        "the stream carries exactly one terminal — the normal factory \
            finalization's; the borrowed driver emits none of its own"
    );
}

/// Frame 9 (S4): a segment re-parking on a newly gated call emits
/// `RunParked` to the caller's channel EXACTLY ONCE, and the
/// coordinator's re-park probe (the E7-era golden's parked-name probe)
/// never duplicates it. The staged shape is the loop re-park:
/// node A completes its decided call, the never-started sibling's
/// continuation issues a fresh gated call, and the run_iteration's
/// park path publishes the checkpoint — the publication owner's
/// single emission rides the borrowed driver's channel, and the
/// coordinator's continuation's parked-name probe observes the same
/// publication without emitting again.
#[tokio::test]
async fn s4_publication_owner_emits_runparked_exactly_once() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let apply_invocations = Arc::new(Mutex::new(Vec::new()));
    let fresh_invocations = Arc::new(Mutex::new(Vec::new()));
    // Build order: node A first (the drive loop), then the sibling's
    // (a build only the resumed coordinator loop can make).
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

    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(64);
    let mut stream = Box::pin(futures::stream::unfold(event_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let end = tokio::time::timeout(
        BORROWED_PROBE,
        run_segment_borrowed(
            &grant,
            &world.config,
            &HashMap::new(),
            event_tx,
            crate::UsageState::new(),
            None,
        ),
    )
    .await
    .expect("the borrowed segment returns within the bound")
    .expect("the segment driver did not panic");

    match &end {
        ResumeStreamEnd::Reparked => {}
        other => panic!("a re-parked segment yields Reparked, got {other:?}"),
    }

    // The drive-loop substitutions ran exactly once each across the
    // segment: node A's decided call executed once; the sibling's fresh
    // gated call never executed its tool.
    assert_eq!(
        apply_invocations
            .lock()
            .expect("apply invocation log")
            .len(),
        1,
        "node A's decided call executes exactly once before the re-park"
    );
    assert!(
        fresh_invocations
            .lock()
            .expect("fresh invocation log")
            .is_empty(),
        "the sibling's freshly gated call never executes its tool"
    );

    let items = drive_to_items(&mut stream).await;
    let parked = run_parked_events(items.as_slice());
    assert_eq!(
        parked.len(),
        1,
        "RunParked rides the borrowed driver's channel EXACTLY once — \
         the publication owner's emission; the coordinator's re-park probe \
         parked-name observation must never duplicate it (found {parked:?})"
    );
    assert!(
        carries_no_final(items.as_slice()),
        "a re-parked segment carries no Final — the driver emits none of its own"
    );
}

/// Frame 10 (S4): THE RENDEZVOUS GOLDEN — the fence's lease reference
/// (from the conversion's held reservation) survives the STREAM BOUNDARY
/// and is released only at the supervisor's drain completion, observed
/// through the shared reservation table across the real ownership chain.
///
/// The frame drives the FACTORY supervisor (`resume_stream_with_timeout`),
/// not the borrowed driver directly: the grant is consumed by value and
/// the test holds NOTHING but the factory's returned surface, so the
/// fence-live assertion below cannot be satisfied by any reference the
/// caller still owns — it is held by the supervisor's internal ownership
/// chain (the grant's lease through the segment, plus the tracked tail
/// the decided call's tool registered through the grant's ONE scope).
/// With the tail in flight at the stream boundary (its gate closed), the
/// run stays reserved; release happens only when the drain completes,
/// never on drop: release-at-drop is falsified for the supervisor-owned
/// fence.
///
/// Review-covered residual (stated, not asserted): the specific lease
/// clone `convert_reserved` moves into its blocking rename tail lives
/// only until that rename completes — a cfg(test) rendezvous INSIDE
/// `convert_reserved` (evaluate.rs:564) would be needed to assert the
/// clone's own binding shape at runtime, and that seam is the recorded
/// residual (goldens.rs:1636's note). This frame pins the property that
/// survives without production visibility: the fence releases at the
/// supervisor's drain end through the real ownership chain, and the
/// conversion's held reservation never drops before the segment's own
/// fences have.
#[tokio::test]
async fn s4_conversion_tail_lease_clone_holds_until_the_supervisor_drain_ends() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    register_decided(&world).await;
    publish_document(&world, &sentinel_document(&world)).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants");
    let run = grant.run_id().clone();
    // The tail must register through the grant's ONE scope, so capture
    // the scope handle before the supervisor consumes the grant.
    let scope = grant.execution_scope();
    let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (release, gate) = tokio::sync::oneshot::channel::<()>();
    let gate = Arc::new(std::sync::Mutex::new(Some(gate)));
    install_worker_overrides(vec![WorkerOverride {
        model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
        extra_tools: vec![Box::new(SupervisorGatedTailTool::new(
            scope,
            Arc::clone(&gate),
            Arc::clone(&spawned),
            Arc::clone(&finished),
        ))],
    }]);
    install_coordinator_overrides(vec![CoordinatorOverride {
        model: ScriptedCompletionModel::new(vec![coordinator_direct_turn()]),
    }]);

    // The supervisor consumes the grant BY VALUE: no lease reference of
    // the conversion survives in the test's hands — the fence below is
    // held only by the supervisor's internal ownership chain.
    let factory = OrchestratorFactory::new(world.config.clone())
        .with_reservation_table(Arc::new(ResumeClaimTable::new()));
    let (mut stream, _cancel_tx, _usage) = factory
        .resume_stream_with_timeout(grant, Duration::from_secs(600), "req_rendezvous")
        .await;

    // The frame's lifecycle, state by state:
    // (i) TODAY (pre-#21): the factory call above panics at the S3 todo —
    //     this point is never reached.
    // (ii) AFTER hole #21's fill, BEFORE hole #12's: the supervisor task
    //     dies at the S4 todo (evaluate.rs:981) before any terminal rides
    //     the stream — the consumer side sees an EARLY CLOSE. Detect it
    //     and FAIL with the explicit message naming the unfilled driver;
    //     never hang, never assert over a partial set.
    // (iii) AFTER hole #12's fill: the honest supervisor holds the stream
    //     open while its drain waits on the held tail — the boundary
    //     assertions below hold, then the release completes the drain.
    let deadline = std::time::Instant::now() + BORROWED_PROBE;
    loop {
        // The held tail must reach its in-flight hold, bounded, with the
        // stream's own termination checked on every poll: an EARLY CLOSE
        // with no terminal is state (ii) and fails here, never hangs.
        assert!(
            !futures::poll!(stream.next()).is_ready(),
            "the factory stream terminated with no terminal having ridden it: \
             the supervisor died before the segment boundary — the S4 borrowed \
             driver (evaluate.rs:981, hole #12) is UNFILLED and must land \
             before this frame can pin its boundary"
        );
        if spawned.load(std::sync::atomic::Ordering::Acquire) >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the supervisor's held tail never reached its in-flight hold within \
             the bound"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // (iii) The stream boundary with the drain still waiting on the
    // in-flight tail: the supervisor holds the stream OPEN (no terminal
    // can ride it while the drain pends — the same shape frame 7 legs
    // pin; a skip-drain shortcut's early close fails here), and the
    // fence is still live. The blocked-window runs CONCURRENTLY with
    // the hold: it asserts stream-pending + fence-live on every
    // iteration for a bounded run of the window, then EXITS — it never
    // waits for the hold to end. Releasing the tail follows and
    // completes the drain.
    for _ in 0..20 {
        assert!(
            !futures::poll!(stream.next()).is_ready(),
            "the factory stream terminated while its drain still waited on the \
             in-flight tail: the exit arm drained late or skipped its \
             tracked-child join"
        );
        assert!(
            world.claims.is_live(&run),
            "the fence survived to the stream boundary — the supervisor's lease \
             held it across every stream item"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    release.send(()).expect("the gated tail is still held");
    // The release completes the drain: the tail's own completion moment
    // (its `finished` observable, bounded) precedes the fence's release.
    let release_deadline = std::time::Instant::now() + BORROWED_PROBE;
    while finished.load(std::sync::atomic::Ordering::Acquire) < 1 {
        assert!(
            std::time::Instant::now() < release_deadline,
            "the released tail never completed within the bound"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while world.claims.is_live(&run) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !world.claims.is_live(&run),
        "the released tail ends the supervisor's drain — the fence releases only \
         after every reference the conversion handed out has ended"
    );
}

/// Frame 11 (S4): the resumed coordinator increments a fresh local cycle
/// counter before each iteration; three fresh cycles are permitted and
/// the fourth fresh cycle stops. The fixture's checkpoint deliberately
/// carries a high historical iteration — evidence-only numbering, so the
/// counter the resumed loop bounds is the fresh one: the resumed run
/// still consumes three full coordinator decision turns (then stops at
/// the fourth) regardless of the checkpoint carrying iteration 8.
#[tokio::test]
async fn s4_coordinator_cycle_counter_allows_three_fresh_cycles() {
    let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
    let _drain = OverrideDrain;
    let world = world();
    let coordinator = ScriptedCompletionModel::new(vec![
        planning_turn_for("cycle one"),
        planning_turn_for("cycle two"),
        planning_turn_for("cycle three"),
        planning_turn_for("the fourth fresh cycle must never be requested"),
    ]);
    let coordinator_requests = coordinator.requests();
    install_coordinator_overrides(vec![CoordinatorOverride { model: coordinator }]);
    // Iteration 1 executes the restored plan (node A's awaiting node,
    // already complete on the drive loop's side); each permitted fresh
    // cycle drives its own `operations` plan, so each fresh build gets
    // its own override — an exhausted queue is never a passage.
    install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![ScriptedTurn::text(FINAL_TEXT)]),
            extra_tools: vec![Box::new(
                RecordingTool::new(Arc::new(Mutex::new(Vec::new()))).with_name(TOOL),
            )],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_fresh_1",
                    "submit_result",
                    json!({
                        "summary": "fresh 1 done",
                        "result": "fresh task one applied",
                        "confidence": "high",
                    }),
                )])
                .with_text("fresh one done"),
            ]),
            extra_tools: vec![],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_fresh_2",
                    "submit_result",
                    json!({
                        "summary": "fresh 2 done",
                        "result": "fresh task two applied",
                        "confidence": "high",
                    }),
                )])
                .with_text("fresh two done"),
            ]),
            extra_tools: vec![],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_sub_fresh_3",
                    "submit_result",
                    json!({
                        "summary": "fresh 3 done",
                        "result": "fresh task three applied",
                        "confidence": "high",
                    }),
                )])
                .with_text("fresh three done"),
            ]),
            extra_tools: vec![],
        },
    ]);
    register_decided(&world).await;
    // The fixture: everything standard, but the checkpoint carries a
    // HIGH historical iteration — the resumed loop's counter is the
    // FRESH one it increments, never the historical count.
    let mut document = sentinel_document(&world);
    document.iteration = 8;
    publish_document(&world, &document).await;

    let grant = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect("the all-decided run grants despite the checkpoint's iteration");

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(64);
    let end = run_segment_borrowed(
        &grant,
        &world.config,
        &HashMap::new(),
        event_tx,
        crate::UsageState::new(),
        None,
    )
    .await
    .expect("the resumed run completes and never dead-ends on history");
    assert!(
        matches!(end, ResumeStreamEnd::Completed { .. }),
        "the resumed run completes under the fresh counter: {end:?}"
    );

    // Three fresh cycles permitted; the fourth (the fourth scripted
    // coordinator turn) is never requested.
    let recorded = coordinator_requests
        .lock()
        .expect("coordinator request log")
        .len();
    assert_eq!(
        recorded, 3,
        "the resumed coordinator runs three FRESH cycles and stops at the \
         fourth request — the checkpoint carrying iteration 8 must neither \
         shorten nor extend the fresh budget (found {recorded})"
    );
}

/// (test 9, repair-round addition) BOTH bundle members decided through the
/// store — real recorded approvals on the two-call bundle's tickets, no
/// fabrication — and the caller's `now` past the document's retention
/// stamp, with the caller inside nothing else. The checkpoint must still
/// expire: the terminal `409 expired` conflict row, the parked checkpoint
/// unlinked, the durable decided terminal records retained as evidence, and
/// a retried resume of the same run answers the absent row — never a ready
/// grant.
///
/// RED today: the all-decided interim consult never checks the retention
/// stamp and returns the ready grant. (Test 4's decided+timed-out mix runs
/// the teardown arms today because the interim consult reads the timed-out
/// member as undecided; this frame is the leg that does not.)
#[tokio::test]
async fn consult_an_all_decided_retention_expired_checkpoint_expires_and_tears_down() {
    let world = world();
    register_decided_pivot_pair(
        &world,
        ApprovalDecision::Approved,
        ApprovalDecision::Approved,
    )
    .await;
    publish_document(&world, &two_call_bundle_document(&world, PAST_STAMP)).await;

    let refusal = evaluate_resume(evaluation(&world, false, None))
        .await
        .expect_err(
            "the retention-expired all-decided checkpoint must expire and tear \
             down, never grant — RED today: the all-decided interim consult \
             never checks the retention stamp and returns the ready grant",
        );
    assert_conflict(
        refusal,
        json!({
            "code": "expired",
            "detail": EXPIRED_DETAIL,
            "blocking": [],
        }),
    );
    assert!(
        !parked_document_path(&world).exists() && !resuming_document_path(&world).exists(),
        "the expired teardown unlinked the parked checkpoint"
    );
    // Retained-evidence semantics: both members decided through the store,
    // so both wrote a durable decision file. The file store's cancel sweep
    // excludes any id whose decision file is present (session_store/file.rs
    // `cancel_request_sync`'s `stale_decided` branch), so the expired
    // teardown leaves the terminal records readable and `try_parked` still
    // answers `Some`.
    for id in [decision(), decision_pivot_2()] {
        assert!(
            world
                .registry
                .try_parked(&id)
                .await
                .expect("the store reads")
                .is_some(),
            "the durable terminal record for {id} remains retained evidence"
        );
    }
    let retry = evaluate_resume(evaluation(&world, false, None)).await;
    assert!(
        matches!(retry, Err(ResumeRefusal::DocumentAbsent)),
        "a retried resume of the same run answers the absent row: {retry:?}"
    );
}
