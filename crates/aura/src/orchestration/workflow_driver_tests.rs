use super::*;
use crate::config::AgentRuntimeConfig;
use crate::orchestration::workflow::{WorkflowCommand, WorkflowRequest};
use aura_config::workflow::WorkflowStage;

fn config(root: &str, request: WorkflowRequest, stages: Value) -> AgentRuntimeConfig {
    AgentRuntimeConfig {
        memory_dir: Some(root.into()),
        session_id: Some("test-session".into()),
        workflow_request: Some(request),
        orchestration: Some(serde_json::from_value(json!({
            "enabled":true,
            "worker":{"worker":{"description":"test", "preamble":"Submit the requested result", "mcp_filter":[], "turn_depth":8}},
            "stages":stages
        })).unwrap()),
        ..Default::default()
    }
}

fn wait_stages(seconds: u64) -> Value {
    json!([{"id":"wait", "worker":"worker", "output_schema":{"type":"object", "required":["waited"]}, "operation":{"kind":"wait", "seconds":seconds}}])
}

async fn run(config: AgentRuntimeConfig, query: &str) -> RunRecord {
    let orchestrator = Orchestrator::new(config).await.unwrap();
    let (events, _rx) = tokio::sync::mpsc::channel(1024);
    let result = orchestrator.run_workflow(query, &events).await.unwrap();
    serde_json::from_str(&result).unwrap()
}

#[tokio::test]
async fn completed_run_deduplicates_and_conflicting_message_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let request = WorkflowRequest::start("start");
    let cfg = config(
        root.path().to_str().unwrap(),
        request.clone(),
        wait_stages(0),
    );
    let first = run(cfg.clone(), "original").await;
    assert!(matches!(first.state, RunState::Completed));
    let replay = run(cfg.clone(), "original").await;
    assert_eq!(first.events.len(), replay.events.len());
    let orchestrator = Orchestrator::new(cfg).await.unwrap();
    let (events, _rx) = tokio::sync::mpsc::channel(16);
    assert!(
        orchestrator
            .run_workflow("changed", &events)
            .await
            .unwrap_err()
            .to_string()
            .contains("reused")
    );
}

#[tokio::test]
async fn inspection_and_takeover_work_during_a_durable_wait() {
    let root = tempfile::tempdir().unwrap();
    let request = WorkflowRequest::start("start");
    let cfg = config(
        root.path().to_str().unwrap(),
        request.clone(),
        wait_stages(60),
    );
    let active = tokio::spawn(run(cfg.clone(), "input"));
    let scope = json!([cfg.agent.name, cfg.session_id]).to_string();
    let reader = RunStore::reader(root.path().to_str().unwrap(), &scope, request.run_id);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if reader
                .load()
                .await
                .unwrap()
                .is_some_and(|r| matches!(r.state, RunState::Waiting { .. }))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut inspect = cfg.clone();
    inspect.workflow_request = Some(WorkflowRequest {
        command: WorkflowCommand::Inspect,
        message_id: "inspect".into(),
        ..request.clone()
    });
    assert!(matches!(
        run(inspect, "").await.state,
        RunState::Waiting { .. }
    ));
    let mut takeover = cfg;
    takeover.workflow_request = Some(WorkflowRequest {
        command: WorkflowCommand::Takeover,
        message_id: "takeover".into(),
        ..request
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run(takeover, ""))
        .await
        .unwrap();
    assert!(
        matches!(result.state, RunState::HumanOwned { suspended } if matches!(*suspended, RunState::Waiting { .. }))
    );
    active.await.unwrap();
}

#[tokio::test]
async fn restart_does_not_replay_an_interrupted_invocation() {
    let root = tempfile::tempdir().unwrap();
    let request = WorkflowRequest::start("start");
    let cfg = config(
        root.path().to_str().unwrap(),
        request.clone(),
        wait_stages(0),
    );
    let mut record = run(cfg.clone(), "input").await;
    record.stage = 0;
    record.transition(RunState::Running, "simulated crash after dispatch");
    let scope = json!([cfg.agent.name, cfg.session_id]).to_string();
    let store = RunStore::open(root.path().to_str().unwrap(), &scope, request.run_id)
        .await
        .unwrap();
    store.save(&record).await.unwrap();
    drop(store);
    let mut resume = cfg;
    resume.workflow_request = Some(WorkflowRequest {
        command: WorkflowCommand::Resume,
        message_id: "resume".into(),
        ..request
    });
    let recovered = run(resume, "").await;
    assert!(matches!(recovered.state, RunState::Uncertain));
    assert_eq!(recovered.stage, 0);
}

#[tokio::test]
async fn schema_corrected_worker_handoff_drives_the_next_configured_stage() {
    use crate::orchestration::test_rig::{
        self, ScriptedCompletionModel, ScriptedToolCall, ScriptedTurn, WorkerOverride,
    };
    let _guard = super::super::tests::WORKER_OVERRIDE_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path().to_str().unwrap(),
        WorkflowRequest::start("workers"),
        json!([
            {"id":"investigate", "worker":"worker", "output_schema":{"type":"object", "required":["count"],"properties":{"count":{"type":"integer"}}}, "operation":{"kind":"worker", "prompt":"Find count"}},
            {"id":"decide", "worker":"worker", "inputs":{"evidence":"/stages/investigate"}, "output_schema":{"type":"object", "required":["action"]}, "operation":{"kind":"worker", "prompt":"Choose action"}}
        ]),
    );
    let submission = |id: &str, result: Value| {
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            id,
            "submit_result",
            json!({"summary":"test", "result":result, "confidence":"high"}),
        )])
    };
    test_rig::install_worker_overrides(vec![
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                submission("bad", json!({"count":"wrong"})),
                submission("good", json!({"count":2})),
                ScriptedTurn::text("done"),
            ]),
            extra_tools: vec![],
        },
        WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                submission("decision", json!({"action":"inspect"})),
                ScriptedTurn::text("done"),
            ]),
            extra_tools: vec![],
        },
    ]);
    let result = run(cfg, "input").await;
    assert!(matches!(result.state, RunState::Completed));
    assert_eq!(result.results["investigate"]["count"], 2);
    assert_eq!(result.results["decide"]["action"], "inspect");
}

#[test]
fn decoded_errors_and_uncertainty_are_preserved() {
    assert!(
        decode_receipt(&serde_json::to_string("Tool returned an error: unavailable").unwrap())
            .is_err()
    );
    let state = RunState::HumanOwned {
        suspended: Box::new(RunState::Cancelled {
            execution_uncertain: true,
        }),
    };
    assert!(state.execution_uncertain());
}

#[tokio::test]
async fn worker_receipt_recovery_preserves_json_looking_strings() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path().to_str().unwrap(),
        WorkflowRequest::start("strings"),
        wait_stages(0),
    );
    let mut record = run(cfg.clone(), "input").await;
    record.stage = 0;
    let orchestrator = Orchestrator::new(cfg.clone()).await.unwrap();
    let scope = json!([cfg.agent.name, cfg.session_id]).to_string();
    let store = RunStore::open(root.path().to_str().unwrap(), &scope, record.run_id)
        .await
        .unwrap();
    let (events, _rx) = tokio::sync::mpsc::channel(16);
    let stage: WorkflowStage = serde_json::from_value(json!({"id":"worker", "worker":"worker", "output_schema":{"type":"string"}, "operation":{"kind":"worker", "prompt":"unused"}})).unwrap();
    for text in ["42", "{}", "null"] {
        record.state = RunState::Received {
            output: serde_json::to_string(text).unwrap(),
        };
        let StageResult::Complete(value) = orchestrator
            .run_stage(&stage, &mut record, &store, &events)
            .await
            .unwrap()
        else {
            panic!("receipt did not complete");
        };
        assert_eq!(value, Value::String(text.into()));
    }
}

#[tokio::test]
async fn notification_receipt_restores_suspended_wait_without_resending() {
    let root = tempfile::tempdir().unwrap();
    let request = WorkflowRequest::start("notification");
    let cfg = config(
        root.path().to_str().unwrap(),
        request.clone(),
        wait_stages(0),
    );
    let mut record = run(cfg.clone(), "input").await;
    record.stage = 0;
    record.pending_notification = Some(crate::orchestration::workflow::PendingNotification {
        key: "0:0".into(),
        suspended: Box::new(RunState::Waiting {
            wake_at: Utc::now(),
            deadline: None,
        }),
    });
    record.state = RunState::Received {
        output: json!({"delivered":true}).to_string(),
    };
    let scope = json!([cfg.agent.name, cfg.session_id]).to_string();
    let store = RunStore::open(root.path().to_str().unwrap(), &scope, record.run_id)
        .await
        .unwrap();
    store.save(&record).await.unwrap();
    drop(store);
    let mut resume = cfg;
    resume.workflow_request = Some(WorkflowRequest {
        command: WorkflowCommand::Resume,
        message_id: "recover".into(),
        ..request
    });
    for (index, state, command, expected) in [
        (
            0,
            record.state.clone(),
            WorkflowCommand::Resume,
            "completed",
        ),
        (
            1,
            RunState::HumanOwned {
                suspended: Box::new(record.state.clone()),
            },
            WorkflowCommand::Resume,
            "completed",
        ),
        (
            2,
            RunState::Received {
                output: json!("Tool returned an error: delivery failed").to_string(),
            },
            WorkflowCommand::Cancel,
            "cancelled",
        ),
        (
            3,
            RunState::Received {
                output: json!("Tool returned an error: delivery failed").to_string(),
            },
            WorkflowCommand::Resume,
            "failed",
        ),
    ] {
        record.state = state;
        let store = RunStore::open(root.path().to_str().unwrap(), &scope, record.run_id)
            .await
            .unwrap();
        store.save(&record).await.unwrap();
        drop(store);
        let mut cfg = resume.clone();
        cfg.workflow_request.as_mut().unwrap().command = command;
        cfg.workflow_request.as_mut().unwrap().message_id = format!("recover-{index}");
        let recovered = run(cfg, "").await;
        assert_eq!(
            serde_json::to_value(&recovered.state).unwrap()["state"],
            expected
        );
        if expected == "completed" {
            assert_eq!(recovered.notifications, ["0:0"]);
            assert!(recovered.pending_notification.is_none());
        }
    }
}
