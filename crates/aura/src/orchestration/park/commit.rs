//! The park commit: awaiting-set refresh, publication, and the
//! no-checkpoint cancellation sweep.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::AgentRuntimeConfig;
use crate::hitl::{DecisionId, PendingApprovals};
use crate::orchestration::persistence::is_safe_path_component;
use crate::orchestration::types::{PendingCall, Plan, TaskState};

use super::ParkedTaskRecords;
use super::document::{
    PARKED_DOCUMENT_SUFFIX, ParkedRun, RESUMING_DOCUMENT_SUFFIX, RunStateForPark, build_document,
};

/// The inputs the orchestrator hands the park commit.
pub(crate) struct ParkCommitInputs<'a> {
    pub state: RunStateForPark<'a>,
    pub plan: &'a Plan,
    pub records: &'a ParkedTaskRecords,
    pub registry: &'a PendingApprovals,
    pub memory_dir: &'a str,
    pub config: &'a AgentRuntimeConfig,
    /// Decision window stamped on a document with no surviving ticket.
    pub decision_window: std::time::Duration,
}

/// The refreshed awaiting set: per-task pending calls still parked —
/// including calls decided since the gate hit, retained for the resume
/// consult — and the earliest expiry among the undecided tickets.
pub(crate) struct RefreshedAwaiting {
    pub pending_by_task: HashMap<usize, Vec<PendingCall>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Every decision id still awaiting a decision, in plan order.
    pub decision_ids: Vec<DecisionId>,
}

/// A completed park commit: the resolved expiry stamp and the refreshed
/// awaiting set.
pub(crate) struct ParkCommitOutcome {
    /// RFC 3339 expiry timestamp.
    pub expires_at: String,
    pub refreshed: RefreshedAwaiting,
}

/// The run-scoped owner id every approval parked by `run_id` is registered
/// under.
pub(crate) fn run_owner_id(run_id: &str) -> String {
    format!("run:{run_id}")
}

/// Narrow the plan's awaiting tasks to the calls still parked, with the
/// earliest surviving ticket expiry. A call decided between gate-hit and
/// commit stays in the checkpoint — the resume consult consumes its
/// recorded decision — but its settled ticket contributes neither expiry
/// nor an outstanding id. A store fault fails the refresh, and with it the
/// commit, rather than dropping a still-decidable approval from the
/// checkpoint.
pub(crate) async fn refresh_awaiting(
    plan: &Plan,
    registry: &PendingApprovals,
) -> io::Result<RefreshedAwaiting> {
    let mut pending_by_task = HashMap::new();
    let mut expires_at = None;
    let mut decision_ids = Vec::new();

    for task in &plan.tasks {
        let TaskState::AwaitingApproval { pending } = &task.state else {
            continue;
        };
        let mut surviving = Vec::with_capacity(pending.len());
        for call in pending {
            let Some(parked) = registry
                .try_parked(&call.decision_id)
                .await
                .map_err(io::Error::other)?
            else {
                tracing::info!(
                    decision_id = %call.decision_id,
                    task_id = task.id,
                    "park approval no longer parked at commit time; dropping from checkpoint",
                );
                continue;
            };
            if registry
                .recorded_decision(&call.decision_id)
                .await
                .is_some()
            {
                tracing::info!(
                    decision_id = %call.decision_id,
                    task_id = task.id,
                    "park approval decided before commit; retaining for the resume consult",
                );
                surviving.push(call.clone());
                continue;
            }
            expires_at = Some(match expires_at {
                Some(earliest) if parked.expires_at >= earliest => earliest,
                _ => parked.expires_at,
            });
            surviving.push(call.clone());
            decision_ids.push(call.decision_id);
        }
        if !surviving.is_empty() {
            pending_by_task.insert(task.id, surviving);
        }
    }

    Ok(RefreshedAwaiting {
        pending_by_task,
        expires_at,
        decision_ids,
    })
}

/// The whole commit: refresh the awaiting set against the store, build the
/// document, publish it. `expires_at` is resolved once here so the document
/// and the caller's terminal event carry the same stamp; a refresh with no
/// surviving ticket stamps `now + decision_window`.
pub(crate) async fn commit_from_run_state(
    inputs: &ParkCommitInputs<'_>,
) -> io::Result<ParkCommitOutcome> {
    let ParkCommitInputs {
        state,
        plan,
        records,
        registry,
        memory_dir,
        config,
        decision_window,
    } = inputs;

    let refreshed = refresh_awaiting(plan, registry).await?;
    let expires_at = refreshed
        .expires_at
        .unwrap_or_else(|| {
            chrono::Utc::now()
                + chrono::Duration::from_std(*decision_window).expect("decision window fits chrono")
        })
        .to_rfc3339();
    let document = build_document(
        state,
        plan,
        records,
        &refreshed.pending_by_task,
        expires_at.clone(),
        config_fingerprint(config),
    )?;
    let parked_dir = parked_document_dir(memory_dir, state.session_id);
    publish(&document, &parked_dir, state.run_id).await?;
    Ok(ParkCommitOutcome {
        expires_at,
        refreshed,
    })
}

/// Publish a checkpoint by temp write and same-directory rename.
///
/// Writes `{parked_dir}/.{run_id}.tmp`, renames it to
/// `{parked_dir}/{run_id}.json`, then removes `{run_id}.resuming.json` if a
/// stale one shadows the fresh document. Publish-then-unlink is the
/// fail-safe order: a crash between the two leaves a correct fresh
/// checkpoint shadowed by a stale resuming document, which reads as
/// interrupted rather than corrupt.
pub(crate) async fn publish(
    document: &ParkedRun,
    parked_dir: &Path,
    run_id: &str,
) -> io::Result<PathBuf> {
    if !is_safe_path_component(run_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid run id for parked document: {run_id:?}"),
        ));
    }
    let bytes = serde_json::to_vec_pretty(document)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // The whole write-rename-unlink sequence runs as one blocking task:
    // the fail-safe order stays a single unit, and no executor thread
    // performs file I/O.
    let parked_dir = parked_dir.to_path_buf();
    let run_id = run_id.to_string();
    tokio::task::spawn_blocking(move || {
        crate::session_store::private_dir(&parked_dir)?;

        let tmp = parked_dir.join(format!(".{run_id}.tmp"));
        crate::session_store::write_private(&tmp, &bytes)?;
        let dest = parked_dir.join(format!("{run_id}{PARKED_DOCUMENT_SUFFIX}"));
        std::fs::rename(&tmp, &dest)?;

        let resuming = parked_dir.join(format!("{run_id}{RESUMING_DOCUMENT_SUFFIX}"));
        if let Err(e) = std::fs::remove_file(&resuming)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %resuming.display(),
                error = %e,
                "failed to unlink stale resuming document after publish",
            );
        }
        Ok(dest)
    })
    .await
    .map_err(io::Error::other)?
}

/// Cancel every approval the run parked: `cancel_request` clears the run's
/// tickets by owner id, and each returned ticket publishes one
/// `approval_completed(cancelled)`. The sweep runs as its own task, so a
/// dropped handle cannot abandon publication mid-flight; await it for
/// ordered teardown. A decided ticket is never in the cleared set, so the
/// stream cannot disagree with a decision that won the race. A lost store
/// reply yields warn-and-empty, the conceded residual.
pub(crate) fn cancel_run_approvals(
    registry: &PendingApprovals,
    run_id: &str,
    request_id: &str,
) -> tokio::task::JoinHandle<()> {
    let registry = registry.clone();
    let run_id = run_id.to_string();
    let request_id = request_id.to_string();
    tokio::task::spawn(async move {
        for parked in registry.cancel_request(&run_owner_id(&run_id)).await {
            crate::approval_event_broker::publish(
                &request_id,
                crate::approval_event_broker::ApprovalLifecycleEvent::Completed(
                    crate::hitl::completed_cancelled(
                        parked.request.decision_id,
                        &parked.request.scope,
                        std::time::Duration::ZERO,
                    ),
                ),
            )
            .await;
        }
    })
}

/// The directory checkpoint documents live in:
/// `{memory_dir}/{session_id}/parked`, or `{memory_dir}/parked` without a
/// session.
pub(crate) fn parked_document_dir(memory_dir: &str, session_id: Option<&str>) -> PathBuf {
    let root = Path::new(memory_dir);
    match session_id {
        Some(sid) => root.join(sid).join("parked"),
        None => root.join("parked"),
    }
}

/// Fingerprint the configuration a resume must not drift from: the HITL
/// gating surface (globs, route, park flag), the agent's model and tool
/// filter, and the per-worker model and tool configuration.
///
/// The webhook route's projection carries `"delivery"` derived from the
/// client's poll marker — the same source `park_registry` reads; no second
/// delivery flag exists. Poll tuning (poll_url, interval, per-attempt
/// timeout) is deliberately fingerprint-COMPATIBLE, and no credential value
/// (headers, secrets) enters the projection; resume-side enforcement is a
/// one-way bump on change.
pub(crate) fn config_fingerprint(config: &AgentRuntimeConfig) -> String {
    let hitl = config.hitl.as_ref();
    let route = hitl.map(|h| match &*h.route {
        crate::hitl::DecisionRoute::Conversational { timeout, .. } => json!({
            "kind": "conversational",
            "timeout_secs": timeout.as_secs(),
        }),
        crate::hitl::DecisionRoute::Webhook {
            client, timeout, ..
        } => json!({
            "kind": "webhook",
            "timeout_secs": timeout.as_secs(),
            "delivery": if client.poll_delivery() { "poll" } else { "sync" },
        }),
    });
    let source = json!({
        "hitl": {
            "patterns": hitl
                .map(|h| h.patterns.iter().map(|p| p.as_str().to_string())
                    .collect::<Vec<_>>())
                .unwrap_or_default(),
            "route": route,
            "park_enabled": hitl.is_some_and(|h| h.park_enabled),
        },
        "agent": {
            "llm": serde_json::to_value(&config.llm).ok(),
            "mcp_filter": &config.agent.mcp_filter,
        },
        "workers": config
            .orchestration
            .as_ref()
            .and_then(|o| serde_json::to_value(&o.workers).ok()),
    });
    let canonical = serde_json::to_string(&source).unwrap_or_else(|_| source.to_string());
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId, PROTOCOL_VERSION,
        ParkedApproval, PendingApprovals, ResolveError,
    };
    use crate::orchestration::park::document::{ParkedPlan, SCHEMA_VERSION, load_parked_run};
    use crate::orchestration::types::Task;
    use crate::session_store::{ApprovalStore, InMemoryApprovalStore, InMemoryEventBus};

    fn conv_registry() -> (PendingApprovals, std::sync::Arc<InMemoryApprovalStore>) {
        let store = std::sync::Arc::new(InMemoryApprovalStore::new());
        let registry = PendingApprovals::with_backend(
            store.clone() as std::sync::Arc<dyn crate::session_store::ApprovalStore>,
            std::sync::Arc::new(InMemoryEventBus::new()),
        );
        (registry, store)
    }

    fn parked_approval(
        decision_id: DecisionId,
        owner: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> ParkedApproval {
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "test-instance".to_string(),
                decision_id,
                request_id: owner.to_string(),
                scope: AgentScope::Single { session_id: None },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "kubectl_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "kubectl_apply".to_string(),
                    arguments: serde_json::json!({ "namespace": "prod" }),
                    tool_call_intent: None,
                }],
            },
            registered_at: chrono::Utc::now(),
            expires_at,
            egress_headers: None,
        }
    }

    /// Publish lands at `{parked}/{run_id}.json`, leaves no temp file, and
    /// unlinks the resuming document a re-park supersedes.
    #[tokio::test]
    async fn publish_renames_and_unlinks_stale_resuming_document() {
        let dir = tempfile::tempdir().unwrap();
        let parked_dir = dir.path().join("parked");
        tokio::fs::create_dir_all(&parked_dir).await.unwrap();
        let run_id = "0191e8c0-cccc-7000-8000-000000000001";
        tokio::fs::write(parked_dir.join(format!("{run_id}.resuming.json")), "stale")
            .await
            .unwrap();

        let document = ParkedRun {
            schema_version: SCHEMA_VERSION,
            session_id: Some("sess".to_string()),
            run_id: run_id.to_string(),
            parked_at: "2026-09-02T14:00:00+00:00".to_string(),
            expires_at: "2026-09-02T15:00:00+00:00".to_string(),
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
                tasks: vec![],
            },
            executed: vec![],
            config_fingerprint: "f".to_string(),
        };

        let dest = publish(&document, &parked_dir, run_id).await.unwrap();
        assert_eq!(dest, parked_dir.join(format!("{run_id}.json")));
        assert!(dest.try_exists().unwrap(), "published document exists");
        assert!(
            !parked_dir
                .join(format!(".{run_id}.tmp"))
                .try_exists()
                .unwrap(),
            "no temp residue"
        );
        assert!(
            !parked_dir
                .join(format!("{run_id}.resuming.json"))
                .try_exists()
                .unwrap(),
            "stale resuming document unlinked"
        );

        let reloaded = load_parked_run(&dest).await.unwrap();
        assert_eq!(reloaded.run_id, run_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the checkpoint is owner-only");
        }
    }

    /// An obstructed temp-file path fails the write and publishes nothing.
    #[tokio::test]
    async fn failing_temp_write_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let parked_dir = dir.path().join("parked");
        tokio::fs::create_dir_all(&parked_dir).await.unwrap();
        let run_id = "0191e8c0-dddd-7000-8000-000000000002";
        let document = ParkedRun {
            schema_version: SCHEMA_VERSION,
            session_id: None,
            run_id: run_id.to_string(),
            parked_at: "2026-09-02T14:00:00+00:00".to_string(),
            expires_at: "2026-09-02T15:00:00+00:00".to_string(),
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
                tasks: vec![],
            },
            executed: vec![],
            config_fingerprint: "f".to_string(),
        };

        let tmp = parked_dir.join(format!(".{run_id}.tmp"));
        tokio::fs::create_dir(&tmp).await.unwrap();
        let result = publish(&document, &parked_dir, run_id).await;

        assert!(result.is_err(), "the temp write must fail");
        assert!(
            !parked_dir
                .join(format!("{run_id}.json"))
                .try_exists()
                .unwrap(),
            "no document is published on a failed temp write"
        );
        assert!(tmp.is_dir(), "the obstruction is left in place");
    }

    /// Refresh drops approvals the store no longer holds, keeps the
    /// undecided ones, and reports the earliest surviving expiry. The memory
    /// backend's resolve moves the row, so a decided call reads as removed
    /// here; the file-backed retention case is
    /// [`early_decision_is_retained_and_consumed_at_resume`].
    #[tokio::test]
    async fn refresh_drops_decided_and_takes_earliest_expiry() {
        let (registry, _store) = conv_registry();
        let owner = "run:0191e8c0-eeee-7000-8000-000000000003";
        let now = chrono::Utc::now();
        let decided = DecisionId::generate();
        let removed = DecisionId::generate();
        let earliest = DecisionId::generate();
        let latest = DecisionId::generate();

        registry
            .register_durable(parked_approval(
                decided,
                owner,
                now + chrono::Duration::hours(2),
            ))
            .await
            .unwrap();
        registry
            .register_durable(parked_approval(
                removed,
                owner,
                now + chrono::Duration::hours(2),
            ))
            .await
            .unwrap();
        registry
            .register_durable(parked_approval(
                earliest,
                owner,
                now + chrono::Duration::minutes(30),
            ))
            .await
            .unwrap();
        registry
            .register_durable(parked_approval(
                latest,
                owner,
                now + chrono::Duration::hours(1),
            ))
            .await
            .unwrap();
        registry
            .resolve(&decided, crate::hitl::ApprovalDecision::Approved.into())
            .await
            .unwrap();
        registry.remove(&removed).await;

        let mut plan = Plan::new("Deploy");
        plan.add_task(Task::new(0, "Gated apply", "r"));
        let pending = vec![
            crate::orchestration::PendingCall {
                decision_id: decided,
                tool_name: "kubectl_apply".to_string(),
                arguments: serde_json::json!({}),
                call_id: "c1".to_string(),
            },
            crate::orchestration::PendingCall {
                decision_id: removed,
                tool_name: "kubectl_delete".to_string(),
                arguments: serde_json::json!({}),
                call_id: "c2".to_string(),
            },
            crate::orchestration::PendingCall {
                decision_id: earliest,
                tool_name: "kubectl_scale".to_string(),
                arguments: serde_json::json!({}),
                call_id: "c3".to_string(),
            },
            crate::orchestration::PendingCall {
                decision_id: latest,
                tool_name: "kubectl_rollout".to_string(),
                arguments: serde_json::json!({}),
                call_id: "c4".to_string(),
            },
        ];
        plan.tasks[0].state = crate::orchestration::TaskState::AwaitingApproval { pending };

        let refreshed = refresh_awaiting(&plan, &registry).await.unwrap();

        let surviving = &refreshed.pending_by_task[&0];
        assert_eq!(surviving.len(), 2, "decided and removed drop out");
        assert_eq!(refreshed.decision_ids, vec![earliest, latest]);
        let reported = refreshed.expires_at.expect("earliest expiry reported");
        let expected = now + chrono::Duration::minutes(30);
        assert!(
            (reported - expected).num_seconds().abs() < 1,
            "expiry is the earliest surviving expiry"
        );
    }

    /// A decision landing between gate-hit and
    /// park commit is retained by the refresh and consumed by the resume
    /// consult. Park mode's file backend keeps the approval readable after
    /// resolve, which is what both sides key on.
    #[tokio::test]
    async fn early_decision_is_retained_and_consumed_at_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ApprovalStore> = Arc::new(
            crate::session_store::FileApprovalStore::open(dir.path().join("approvals")).unwrap(),
        );
        let registry = PendingApprovals::with_backend(store, Arc::new(InMemoryEventBus::new()));

        let run_id = "0191e8c0-cccc-7000-8000-000000000009";
        let owner = run_owner_id(run_id);
        let decided = DecisionId::generate();
        let args = serde_json::json!({ "namespace": "prod" });
        let now = chrono::Utc::now();
        registry
            .register_durable(ParkedApproval {
                request: ApprovalRequest {
                    version: PROTOCOL_VERSION,
                    instance_id: "test-instance".to_string(),
                    decision_id: decided,
                    request_id: owner,
                    scope: AgentScope::Worker {
                        run_id: run_id.parse().unwrap(),
                        task: crate::orchestration::TaskIdentity::new(3, None),
                        session_id: None,
                    },
                    origin: ApprovalOrigin::ConfigGate {
                        matched_pattern: "kubectl_*".to_string(),
                        agent_name: "test-agent".to_string(),
                    },
                    items: vec![ApprovalItem {
                        tool_name: "kubectl_apply".to_string(),
                        arguments: args.clone(),
                        tool_call_intent: None,
                    }],
                },
                registered_at: now,
                expires_at: now + chrono::Duration::hours(1),
                egress_headers: None,
            })
            .await
            .unwrap();
        // The decision wins the race against the park commit.
        registry
            .resolve(&decided, crate::hitl::ApprovalDecision::Approved.into())
            .await
            .unwrap();

        let mut plan = Plan::new("Deploy");
        plan.add_task(Task::new(3, "Gated apply", "r"));
        plan.tasks[0].state = TaskState::AwaitingApproval {
            pending: vec![PendingCall {
                decision_id: decided,
                tool_name: "kubectl_apply".to_string(),
                arguments: args.clone(),
                call_id: "c1".to_string(),
            }],
        };

        let refreshed = refresh_awaiting(&plan, &registry).await.unwrap();
        assert_eq!(
            refreshed.pending_by_task[&3].len(),
            1,
            "the decided call stays in the checkpoint"
        );
        assert!(
            refreshed.decision_ids.is_empty(),
            "a decided call is no longer outstanding"
        );
        assert!(
            refreshed.expires_at.is_none(),
            "a settled ticket bounds nothing"
        );

        // The resume consult consumes the retained call's recorded decision.
        let mut records = ParkedTaskRecords::new();
        records.insert(
            3,
            crate::orchestration::park::ParkedTaskRecord {
                attempt: 1,
                snapshot: crate::orchestration::ParkSnapshot {
                    history: vec![rig::completion::Message::user("apply it")],
                    current_prompt: rig::completion::Message::user("tool results"),
                },
            },
        );
        let document = build_document(
            &RunStateForPark {
                run_id,
                session_id: None,
                query: "Deploy",
                chat_history: &[],
                coordinator_conversation: &[],
                routing_decision: None,
                iteration: 1,
                planning_ms: 0,
                failure_history: &[],
            },
            &plan,
            &records,
            &refreshed.pending_by_task,
            (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
            config_fingerprint(&AgentRuntimeConfig::default()),
        )
        .unwrap();
        let (recorded, ids) =
            crate::orchestration::park::load_recorded_decisions(&registry, &document)
                .await
                .unwrap();
        assert_eq!(ids, vec![decided]);
        assert_eq!(
            recorded.take(&crate::orchestration::CallKey::new(
                3,
                "kubectl_apply",
                &args
            )),
            Some(crate::hitl::ResolvedDecision::from(
                crate::hitl::ApprovalDecision::Approved
            )),
            "the early decision is consumed at resume",
        );
    }

    /// The sweep cancels exactly what the store still holds: the undecided
    /// sibling clears with one event, while the decided sibling — whose
    /// ticket resolve already removed — is absent from the cleared set and
    /// publishes nothing.
    #[tokio::test]
    async fn sweep_publishes_cancelled_for_undecided_sibling_only() {
        let (registry, store) = conv_registry();
        let run_id = "0191e8c0-ffff-7000-8000-000000000006";
        let owner = run_owner_id(run_id);
        let request_id = format!("req_sweep_{}", uuid::Uuid::new_v4().simple());
        let mut events = crate::approval_event_broker::subscribe(&request_id).await;

        let now = chrono::Utc::now();
        let decided = DecisionId::generate();
        let sibling = DecisionId::generate();
        registry
            .register_durable(parked_approval(
                decided,
                &owner,
                now + chrono::Duration::hours(1),
            ))
            .await
            .unwrap();
        registry
            .register_durable(parked_approval(
                sibling,
                &owner,
                now + chrono::Duration::hours(1),
            ))
            .await
            .unwrap();
        registry
            .resolve(&decided, crate::hitl::ApprovalDecision::Approved.into())
            .await
            .unwrap();

        cancel_run_approvals(&registry, run_id, &request_id)
            .await
            .unwrap();

        assert!(
            store.get(&sibling).await.unwrap().is_none(),
            "the undecided sibling's parked entry is cleared"
        );
        assert_eq!(
            registry.recorded_decision(&decided).await,
            Some(crate::hitl::ResolvedDecision::from(
                crate::hitl::ApprovalDecision::Approved
            )),
        );

        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Some(crate::approval_event_broker::ApprovalLifecycleEvent::Completed(
                completed,
            ))) => {
                assert_eq!(completed.decision_id, sibling.to_string());
                assert!(matches!(
                    completed.outcome,
                    aura_events::ApprovalOutcomeWire::Cancelled { .. }
                ));
            }
            other => panic!("expected the sibling's completed(cancelled), got {other:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "the decided approval publishes no cancelled event"
        );

        crate::approval_event_broker::unsubscribe(&request_id).await;
    }

    /// Two tickets under the same owner clear with one cancelled event each.
    #[tokio::test]
    async fn sweep_publishes_one_event_per_cleared_ticket() {
        let (registry, store) = conv_registry();
        let run_id = "0191e8c0-aaaa-7000-8000-000000000007";
        let owner = run_owner_id(run_id);
        let request_id = format!("req_sweep_{}", uuid::Uuid::new_v4().simple());
        let mut events = crate::approval_event_broker::subscribe(&request_id).await;

        let now = chrono::Utc::now();
        let first = DecisionId::generate();
        let second = DecisionId::generate();
        registry
            .register_durable(parked_approval(
                first,
                &owner,
                now + chrono::Duration::hours(1),
            ))
            .await
            .unwrap();
        registry
            .register_durable(parked_approval(
                second,
                &owner,
                now + chrono::Duration::hours(1),
            ))
            .await
            .unwrap();

        cancel_run_approvals(&registry, run_id, &request_id)
            .await
            .unwrap();

        assert!(store.get(&first).await.unwrap().is_none());
        assert!(store.get(&second).await.unwrap().is_none());

        let mut cancelled_ids = Vec::new();
        for _ in 0..2 {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
                Ok(Some(crate::approval_event_broker::ApprovalLifecycleEvent::Completed(
                    completed,
                ))) => cancelled_ids.push(completed.decision_id),
                other => panic!("expected a second completed(cancelled), got {other:?}"),
            }
        }
        assert!(cancelled_ids.contains(&first.to_string()));
        assert!(cancelled_ids.contains(&second.to_string()));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "exactly two cancelled events publish"
        );

        crate::approval_event_broker::unsubscribe(&request_id).await;
    }

    /// A cleared ticket refuses a late resolve: the sweep is terminal for it.
    #[tokio::test]
    async fn late_resolve_after_sweep_is_not_found() {
        let (registry, _store) = conv_registry();
        let run_id = "0191e8c0-bbbb-7000-8000-000000000008";
        let owner = run_owner_id(run_id);
        let now = chrono::Utc::now();
        let ticket = DecisionId::generate();
        registry
            .register_durable(parked_approval(
                ticket,
                &owner,
                now + chrono::Duration::hours(1),
            ))
            .await
            .unwrap();

        cancel_run_approvals(&registry, run_id, "req_late_resolve")
            .await
            .unwrap();

        assert_eq!(
            registry
                .resolve(&ticket, crate::hitl::ApprovalDecision::Approved.into())
                .await,
            Err(ResolveError::NotFound),
        );
    }

    /// The fingerprint is stable for an unchanged config and moves when the
    /// gating or tool surface changes.
    #[test]
    fn config_fingerprint_stable_and_sensitive() {
        use aura_config::GlobPattern;

        fn config(pattern: &str) -> crate::config::AgentRuntimeConfig {
            crate::config::AgentRuntimeConfig {
                hitl: Some(crate::hitl::HitlRuntime {
                    patterns: Arc::from([GlobPattern::new(pattern).unwrap()]),
                    route: Arc::new(crate::hitl::DecisionRoute::Conversational {
                        registry: PendingApprovals::new(),
                        timeout: Duration::from_secs(120),
                    }),
                    park_enabled: true,
                }),
                ..crate::config::AgentRuntimeConfig::default()
            }
        }

        assert_eq!(
            config_fingerprint(&config("kubectl_*")),
            config_fingerprint(&config("kubectl_*")),
            "unchanged config is a stable hash"
        );
        assert_ne!(
            config_fingerprint(&config("kubectl_*")),
            config_fingerprint(&config("helm_*")),
            "a changed gate surface changes the hash"
        );
    }

    /// Webhook routes: the fingerprint carries `"delivery"` from the client's
    /// poll marker, so poll and sync differ, an unchanged webhook config is
    /// stable, and poll TUNING (poll_url, interval, per-attempt timeout)
    /// stays compatible. Credentials (static headers, mapped-header values)
    /// never enter the projection.
    #[test]
    fn config_fingerprint_carries_delivery_and_stays_tuning_compatible() {
        use aura_config::{
            DecisionRouteConfig, GlobPattern, ToolHeaderMappings, WebhookDelivery, WebhookUrl,
        };
        use std::collections::HashMap;

        fn config_with_route(
            route: DecisionRouteConfig,
            patterns: &str,
        ) -> crate::config::AgentRuntimeConfig {
            crate::config::AgentRuntimeConfig {
                hitl: Some(crate::hitl::HitlRuntime {
                    patterns: Arc::from([GlobPattern::new(patterns).unwrap()]),
                    route: Arc::new(match &route {
                        DecisionRouteConfig::Conversational { .. } => {
                            crate::hitl::DecisionRoute::Conversational {
                                registry: PendingApprovals::new(),
                                timeout: Duration::from_secs(120),
                            }
                        }
                        DecisionRouteConfig::Webhook { .. } => {
                            let client =
                                crate::hitl::webhook_client_from_config(&route, None, None)
                                    .expect("a webhook route config builds a client");
                            crate::hitl::DecisionRoute::Webhook {
                                client,
                                registry: PendingApprovals::new(),
                                timeout: Duration::from_secs(300),
                                egress_capture: Ok(()),
                            }
                        }
                    }),
                    park_enabled: true,
                }),
                ..crate::config::AgentRuntimeConfig::default()
            }
        }

        fn webhook_route(
            delivery: WebhookDelivery,
            poll_url: Option<&str>,
            poll_interval_secs: u64,
            poll_request_timeout_secs: u64,
            headers: HashMap<String, String>,
        ) -> DecisionRouteConfig {
            DecisionRouteConfig::Webhook {
                url: WebhookUrl::new("https://approvals.example.com/hook").unwrap(),
                timeout_secs: 300,
                headers,
                headers_from_request: HashMap::new(),
                tool_headers_from_response: ToolHeaderMappings::default(),
                delivery,
                poll_url: poll_url.map(|u| WebhookUrl::new(u).unwrap()),
                poll_interval_secs,
                poll_request_timeout_secs,
            }
        }

        let sync = webhook_route(WebhookDelivery::Sync, None, 10, 30, HashMap::new());
        let poll = webhook_route(WebhookDelivery::Poll, None, 10, 30, HashMap::new());

        assert_eq!(
            config_fingerprint(&config_with_route(poll.clone(), "kubectl_*")),
            config_fingerprint(&config_with_route(poll.clone(), "kubectl_*")),
            "an unchanged webhook config is a stable hash"
        );
        assert_ne!(
            config_fingerprint(&config_with_route(sync.clone(), "kubectl_*")),
            config_fingerprint(&config_with_route(poll.clone(), "kubectl_*")),
            "poll and sync deliveries must fingerprint differently"
        );

        // Poll tuning is fingerprint-compatible: a redeploy that only moves
        // the status endpoint or retunes cadence/timeouts resumes.
        let retuned = webhook_route(
            WebhookDelivery::Poll,
            Some("https://status.example.com/x"),
            45,
            7,
            HashMap::new(),
        );
        assert_eq!(
            config_fingerprint(&config_with_route(poll.clone(), "kubectl_*")),
            config_fingerprint(&config_with_route(retuned, "kubectl_*")),
            "poll tuning must not change the fingerprint"
        );

        // The gate surface still moves the hash on a webhook route.
        assert_ne!(
            config_fingerprint(&config_with_route(poll.clone(), "kubectl_*")),
            config_fingerprint(&config_with_route(poll, "helm_*")),
            "a changed gate surface changes the hash"
        );

        // Credential values are excluded: a route whose static headers carry
        // a secret fingerprints the same as one with none.
        let with_secret = webhook_route(
            WebhookDelivery::Sync,
            None,
            10,
            30,
            HashMap::from([(
                "authorization".to_string(),
                "Bearer credential-value".to_string(),
            )]),
        );
        assert_eq!(
            config_fingerprint(&config_with_route(sync, "kubectl_*")),
            config_fingerprint(&config_with_route(with_secret, "kubectl_*")),
            "credential values must not enter the fingerprint"
        );
    }
}
