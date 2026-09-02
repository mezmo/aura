//! The park commit: awaiting-set refresh, publication, and the
//! no-checkpoint cancellation sweep.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::AgentRuntimeConfig;
use crate::hitl::{AgentScope, DecisionId, PendingApprovals};
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

/// The refreshed awaiting set: per-task pending calls still awaiting a
/// decision, and the earliest expiry among them.
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

/// Narrow the plan's awaiting tasks to the calls still parked and undecided,
/// with the earliest surviving ticket expiry. A store fault fails the
/// refresh, and with it the commit, rather than dropping a still-decidable
/// approval from the checkpoint.
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
                    "park approval decided before commit; dropping from checkpoint",
                );
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
        std::fs::create_dir_all(&parked_dir)?;

        let tmp = parked_dir.join(format!(".{run_id}.tmp"));
        std::fs::write(&tmp, &bytes)?;
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

/// Cancel every undecided approval the run parked: a recorded decision is
/// spared, an id no longer parked is skipped (the sweep is idempotent), and
/// each remaining id publishes one `approval_completed(cancelled)` before the
/// store clears the run's tickets by owner id.
pub(crate) async fn cancel_run_approvals(
    registry: &PendingApprovals,
    run_id: &str,
    request_id: &str,
    cancelled: impl Iterator<Item = (DecisionId, AgentScope)>,
) {
    for (decision_id, scope) in cancelled {
        if registry.recorded_decision(&decision_id).await.is_some() {
            tracing::info!(
                decision_id = %decision_id,
                "approval decided before the cancellation sweep; decision spared",
            );
            continue;
        }
        let parked = registry
            .try_parked(&decision_id)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(decision_id = %decision_id, error = %err, "parked approval lookup failed");
                None
            });
        if parked.is_none() {
            continue;
        }
        crate::approval_event_broker::publish(
            request_id,
            crate::approval_event_broker::ApprovalLifecycleEvent::Completed(
                crate::hitl::completed_cancelled(decision_id, &scope, std::time::Duration::ZERO),
            ),
        )
        .await;
    }
    // The owner id the park arm stamps on every ticket it registers.
    registry.cancel_request(&run_owner_id(run_id)).await;
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
pub(crate) fn config_fingerprint(config: &AgentRuntimeConfig) -> String {
    let hitl = config.hitl.as_ref();
    let route = hitl.map(|h| match &*h.route {
        crate::hitl::DecisionRoute::Conversational { timeout, .. } => json!({
            "kind": "conversational",
            "timeout_secs": timeout.as_secs(),
        }),
        crate::hitl::DecisionRoute::Webhook { timeout, .. } => json!({
            "kind": "webhook",
            "timeout_secs": timeout.as_secs(),
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
        ParkedApproval, PendingApprovals,
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
    }

    /// A read-only parked directory fails the temp write, publishes nothing,
    /// and leaves no partial document.
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

        let mut perms = tokio::fs::metadata(&parked_dir)
            .await
            .unwrap()
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o555);
        tokio::fs::set_permissions(&parked_dir, perms)
            .await
            .unwrap();
        let result = publish(&document, &parked_dir, run_id).await;
        // Restore writability so the tempdir can clean itself up.
        let mut perms = tokio::fs::metadata(&parked_dir)
            .await
            .unwrap()
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        tokio::fs::set_permissions(&parked_dir, perms)
            .await
            .unwrap();

        assert!(result.is_err(), "the temp write must fail");
        assert!(
            !parked_dir
                .join(format!("{run_id}.json"))
                .try_exists()
                .unwrap(),
            "no document is published on a failed temp write"
        );
    }

    /// Refresh drops decided and removed approvals, keeps the undecided
    /// ones, and reports the earliest surviving expiry.
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
            .resolve(&decided, crate::hitl::ApprovalDecision::Approved)
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

    /// The sweep spares an id with a recorded decision — no event, no
    /// removal — while an undecided sibling under the same owner is
    /// cancelled with exactly one event.
    #[tokio::test]
    async fn sweep_spares_recorded_decision_and_cancels_undecided_sibling() {
        let (registry, store) = conv_registry();
        let run_id = "0191e8c0-ffff-7000-8000-000000000006";
        let owner = run_owner_id(run_id);
        let request_id = format!("req_sweep_{}", uuid::Uuid::new_v4().simple());
        let mut events = crate::approval_event_broker::subscribe(&request_id).await;

        let now = chrono::Utc::now();
        let decided = DecisionId::generate();
        let sibling = DecisionId::generate();
        let scope = AgentScope::Single { session_id: None };
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
            .resolve(&decided, crate::hitl::ApprovalDecision::Approved)
            .await
            .unwrap();

        cancel_run_approvals(
            &registry,
            run_id,
            &request_id,
            [(decided, scope.clone()), (sibling, scope)].into_iter(),
        )
        .await;

        assert!(
            store.get(&sibling).await.unwrap().is_none(),
            "the undecided sibling's parked entry is cleared"
        );
        assert_eq!(
            registry.recorded_decision(&decided).await,
            Some(crate::hitl::ApprovalDecision::Approved),
            "the recorded decision survives the sweep"
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
}
