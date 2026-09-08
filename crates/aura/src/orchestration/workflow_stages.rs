//! Stage dispatch, approval continuations, and verification through existing workers.

use std::sync::Arc;

use aura_config::workflow::{StageOperation, WorkflowStage, compile_schema};
use chrono::Utc;
use serde_json::{Value, json};

use super::super::{Orchestrator, TaskExecutionParams, TaskOutcome};
use crate::hitl::{ApprovalDecision, DecisionRoute, PendingApprovals};
use crate::orchestration::park::{
    ParkedPlan, ParkedRun, ParkedTaskNode, ResumeContext, ResumingDocumentHandle, TaskContinuation,
    load_recorded_decisions,
};
use crate::orchestration::types::{BlockedCell, CellOutcome, PendingCall, TaskStatus};
use crate::orchestration::workflow::{RunRecord, RunState, RunStore};
use crate::provider_agent::StreamError;

use super::{Events, StageResult, deadline, decode_receipt, merge_arguments, save_workflow};

impl Orchestrator {
    pub(super) fn validate_workflow_tools(&self) -> Result<(), StreamError> {
        for stage in &self.config.stages {
            match &stage.operation {
                StageOperation::Tool { tool, .. } | StageOperation::Verify { tool, .. } => {
                    self.check_workflow_tool(&stage.worker, tool)?
                }
                _ => {}
            }
            for notification in &stage.on_approval_wait {
                self.check_workflow_tool(&notification.worker, &notification.tool)?;
                if self.agent_config.hitl.as_ref().is_some_and(|hitl| {
                    hitl.patterns
                        .iter()
                        .any(|pattern| pattern.matches(&notification.tool))
                }) {
                    return Err("approval notifications cannot themselves require approval".into());
                }
            }
        }
        Ok(())
    }

    fn check_workflow_tool(&self, worker: &str, name: &str) -> Result<(), StreamError> {
        let manager = self
            .mcp_manager
            .as_ref()
            .ok_or("configured tool requires MCP")?;
        if manager
            .tool_definitions_iter()
            .filter(|tool| tool.name == name)
            .count()
            != 1
        {
            return Err(format!("workflow tool must identify exactly one MCP tool: {name}").into());
        }
        let mut config = self.agent_config.clone();
        if let Some(filter) = self
            .config
            .workers
            .get(worker)
            .and_then(|w| w.mcp_filter.clone())
        {
            config.mcp_filter = Some(filter);
        }
        if !config.tool_matches_filter(name) {
            return Err(format!("worker {worker} cannot call {name}").into());
        }
        Ok(())
    }

    pub(super) async fn run_stage(
        &self,
        stage: &WorkflowStage,
        record: &mut RunRecord,
        store: &RunStore,
        events: &Events,
    ) -> Result<StageResult, StreamError> {
        if let RunState::Received { output } = &record.state
            && !matches!(stage.operation, StageOperation::Verify { .. })
        {
            return Ok(StageResult::Complete(
                if matches!(stage.operation, StageOperation::Worker { .. }) {
                    serde_json::from_str(output)?
                } else {
                    decode_receipt(output)?
                },
            ));
        }
        let source = json!({"input": record.input, "stages": record.results, "run": record});

        let mut inputs = serde_json::Map::new();
        for (key, pointer) in &stage.inputs {
            inputs.insert(
                key.clone(),
                source
                    .pointer(pointer)
                    .ok_or_else(|| format!("missing input {pointer} for stage {}", stage.id))?
                    .clone(),
            );
        }
        let checkpoint = match &record.state {
            RunState::AwaitingApproval { checkpoint } => Some(checkpoint.clone()),
            _ => None,
        };
        let resume = if let Some(checkpoint) = &checkpoint {
            let registry = self.workflow_registry()?;
            let (recorded, _) = match load_recorded_decisions(registry, checkpoint).await {
                Ok(value) => value,
                Err(crate::orchestration::park::RehydrateError::Parked { .. }) => {
                    return Ok(StageResult::Parked(checkpoint.clone()));
                }
                Err(error) => return Err(error.to_string().into()),
            };
            for node in &checkpoint.plan.tasks {
                for call in node.pending.iter().flatten() {
                    if matches!(
                        registry.recorded_decision(&call.decision_id).await,
                        Some(ApprovalDecision::Denied { .. })
                    ) {
                        return Err("human denied the proposed action".into());
                    }
                }
            }
            Some(ResumeContext {
                recorded,
                document: Arc::new(ResumingDocumentHandle::from_document(
                    (**checkpoint).clone(),
                    store.resume_path(),
                )),
            })
        } else {
            None
        };
        if let RunState::Waiting { wake_at, .. } = &record.state
            && let Ok(delay) = (*wake_at - Utc::now()).to_std()
        {
            tokio::time::sleep(delay).await;
        }
        match &stage.operation {
            StageOperation::Wait { seconds } => {
                if !matches!(record.state, RunState::Waiting { .. }) {
                    let wake_at = deadline(*seconds)?;
                    record.transition(
                        RunState::Waiting {
                            wake_at,
                            deadline: None,
                        },
                        "durable wait started",
                    );
                    save_workflow(record, store, events).await?;
                    tokio::time::sleep(std::time::Duration::from_secs(*seconds)).await;
                }
                Ok(StageResult::Complete(json!({"waited": true})))
            }
            StageOperation::Worker { prompt } => {
                let continuation = checkpoint
                    .as_ref()
                    .and_then(|doc| doc.plan.tasks.first())
                    .map(|node| {
                        Ok::<_, StreamError>(TaskContinuation {
                            attempt: node.attempt.ok_or("missing worker attempt")?,
                            history: node.history.clone().ok_or("missing worker history")?,
                            current_prompt: node
                                .current_prompt
                                .clone()
                                .ok_or("missing worker prompt")?,
                            pending: node.pending.clone().ok_or("missing pending calls")?,
                        })
                    })
                    .transpose()?;
                record.transition(RunState::Running, "worker started");
                save_workflow(record, store, events).await?;
                let context = Some(Value::Object(inputs).to_string());
                let params = TaskExecutionParams {
                    task_description: prompt,
                    task_context: &context,
                    worker_name: Some(&stage.worker),
                };
                match self
                    .execute_task(
                        record.stage,
                        &params,
                        Some(events),
                        continuation.as_ref(),
                        resume.as_ref(),
                    )
                    .await?
                {
                    TaskOutcome::Completed(result) => {
                        if result.structured_output.is_none() {
                            return Err(
                                "worker exhausted corrections without a validated submit_result"
                                    .into(),
                            );
                        }
                        let value = serde_json::from_str(&result.result)?;
                        record.transition(
                            RunState::Received {
                                output: result.result,
                            },
                            "worker result recorded",
                        );
                        save_workflow(record, store, events).await?;
                        Ok(StageResult::Complete(value))
                    }
                    TaskOutcome::Blocked {
                        pending,
                        attempt,
                        snapshot,
                    } => Ok(StageResult::Parked(Box::new(
                        self.workflow_checkpoint(record, stage, pending, attempt, Some(snapshot))
                            .await?,
                    ))),
                }
            }
            StageOperation::Tool { tool, arguments } => {
                let arguments = merge_arguments(arguments, inputs)?;
                self.workflow_call(record, store, stage, tool, &arguments, resume.as_ref())
                    .await
            }
            StageOperation::Verify {
                tool,
                arguments,
                success_schema,
                failure_schema,
                interval_secs,
                timeout_secs,
                ..
            } => {
                let arguments = merge_arguments(arguments, inputs)?;
                let deadline_at = *record
                    .verification_deadline
                    .get_or_insert(deadline(*timeout_secs)?);
                let success = compile_schema(success_schema)?;
                let failure = failure_schema.as_ref().map(compile_schema).transpose()?;
                let mut resume = resume;
                loop {
                    if Utc::now() >= deadline_at {
                        return Ok(StageResult::Inconclusive);
                    }
                    let remaining = (deadline_at - Utc::now()).to_std()?;
                    let sample = match tokio::time::timeout(
                        remaining,
                        self.workflow_call(record, store, stage, tool, &arguments, resume.as_ref()),
                    )
                    .await
                    {
                        Ok(result) => result?,
                        Err(_) => return Ok(StageResult::Inconclusive),
                    };
                    resume = None;
                    let StageResult::Complete(value) = sample else {
                        return Ok(sample);
                    };
                    if success.is_valid(&value) {
                        return Ok(StageResult::Complete(value));
                    }
                    if failure
                        .as_ref()
                        .is_some_and(|schema| schema.is_valid(&value))
                    {
                        return Err("verification observed the configured failure condition".into());
                    }
                    let wake_at = deadline(*interval_secs)?.min(deadline_at);
                    record.transition(
                        RunState::Waiting {
                            wake_at,
                            deadline: Some(deadline_at),
                        },
                        "verification pending",
                    );
                    save_workflow(record, store, events).await?;
                    if let Ok(delay) = (wake_at - Utc::now()).to_std() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
    }

    async fn workflow_call(
        &self,
        record: &mut RunRecord,
        store: &RunStore,
        stage: &WorkflowStage,
        tool: &str,
        arguments: &Value,
        resume: Option<&ResumeContext>,
    ) -> Result<StageResult, StreamError> {
        if let RunState::Received { output } = &record.state {
            return Ok(StageResult::Complete(decode_receipt(output)?));
        }
        // Only registered MCP names are accepted, not internal tools such as
        // submit_result, wait_for, or request_approval.
        let manager = self
            .mcp_manager
            .as_ref()
            .ok_or("MCP tool stage requires MCP configuration")?;
        if !manager
            .get_available_tool_names()
            .iter()
            .any(|name| name == tool)
        {
            return Err(format!("unknown MCP tool {tool}").into());
        }
        let cell = Arc::new(BlockedCell::default());
        cell.set_current_call_id(Some(format!("{}:{}", record.run_id, record.stage)));
        let worker = self
            .create_worker(
                record.stage,
                1,
                Some(&stage.worker),
                Some(&cell),
                resume.map(|r| &r.recorded),
                true,
            )
            .await?;
        let _strict = resume.map(|r| r.recorded.strict_guard(record.stage));
        record.transition(
            RunState::Running,
            "tool invocation started; missing receipt requires reconciliation",
        );
        store.save(record).await?;
        let output = worker
            .agent
            .inner
            .call_tool(tool, &arguments.to_string())
            .await?;
        match cell.outcome() {
            CellOutcome::Blocked { pending } | CellOutcome::Orphaned { pending } => {
                return Ok(StageResult::Parked(Box::new(
                    self.workflow_checkpoint(record, stage, pending, 1, None)
                        .await?,
                )));
            }
            CellOutcome::Normal => {}
        }
        record.receipts.push(json!({"stage": record.stage, "tool": tool, "arguments": arguments, "output": output, "at": Utc::now()}));
        record.transition(
            RunState::Received {
                output: output.clone(),
            },
            "tool receipt recorded",
        );
        store.save(record).await?;
        let value: Value = serde_json::from_str(&output)?;

        let value = if let Value::String(text) = value {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        } else {
            value
        };
        if !matches!(
            crate::tool_error_detection::detect_tool_error(
                value.as_str().unwrap_or(&value.to_string())
            ),
            crate::tool_error_detection::ToolResultStatus::Success
        ) {
            return Err("MCP execution returned an error".into());
        }
        Ok(StageResult::Complete(value))
    }

    pub(super) async fn workflow_notifications(
        &self,
        stage: &WorkflowStage,
        record: &mut RunRecord,
        store: &RunStore,
    ) -> Result<(), StreamError> {
        let RunState::AwaitingApproval { checkpoint } = &record.state else {
            return Ok(());
        };
        let parked_at =
            chrono::DateTime::parse_from_rfc3339(&checkpoint.parked_at)?.with_timezone(&Utc);
        for (index, notification) in stage.on_approval_wait.iter().enumerate() {
            let key = format!("{}:{index}", record.stage);
            if record.notifications.contains(&key)
                || (Utc::now() - parked_at).num_seconds() < i64::try_from(notification.after_secs)?
            {
                continue;
            }
            if self.agent_config.hitl.as_ref().is_some_and(|hitl| {
                hitl.patterns
                    .iter()
                    .any(|pattern| pattern.matches(&notification.tool))
            }) {
                return Err("approval notifications cannot themselves require approval".into());
            }
            let mut notification_stage = stage.clone();
            notification_stage.worker = notification.worker.clone();
            notification_stage.operation = StageOperation::Tool {
                tool: notification.tool.clone(),
                arguments: notification.arguments.clone(),
            };
            let suspended = record.state.clone();
            let mut arguments = notification
                .arguments
                .as_object()
                .ok_or("notification arguments must be an object")?
                .clone();
            let source = json!({"input": record.input, "stages": record.results, "run": record});
            for (name, pointer) in &notification.inputs {
                if arguments
                    .insert(
                        name.clone(),
                        source
                            .pointer(pointer)
                            .ok_or("notification input missing")?
                            .clone(),
                    )
                    .is_some()
                {
                    return Err("notification input overwrites an argument".into());
                }
            }
            record.pending_notification =
                Some(crate::orchestration::workflow::PendingNotification {
                    key: key.clone(),
                    suspended: Box::new(suspended.clone()),
                });
            store.save(record).await?;

            match self
                .workflow_call(
                    record,
                    store,
                    &notification_stage,
                    &notification.tool,
                    &Value::Object(arguments),
                    None,
                )
                .await?
            {
                StageResult::Complete(_) => {
                    record.notifications.push(key);
                    record.pending_notification = None;
                    record.transition(suspended, "approval notification delivered");
                    store.save(record).await?;
                }
                _ => return Err("approval notification did not complete".into()),
            }
        }
        Ok(())
    }

    pub(super) fn workflow_registry(&self) -> Result<&PendingApprovals, StreamError> {
        match self.agent_config.hitl.as_ref().map(|h| &*h.route) {
            Some(DecisionRoute::Conversational { registry, .. }) => Ok(registry),
            _ => Err("durable workflow approvals require the conversational approval store".into()),
        }
    }

    async fn workflow_checkpoint(
        &self,
        record: &RunRecord,
        stage: &WorkflowStage,
        pending: Vec<PendingCall>,
        attempt: usize,
        snapshot: Option<crate::orchestration::ParkSnapshot>,
    ) -> Result<ParkedRun, StreamError> {
        let registry = self.workflow_registry()?;
        let mut expires = None;
        for call in &pending {
            let approval = registry
                .try_parked(&call.decision_id)
                .await?
                .ok_or("parked approval missing from store")?;
            expires = Some(
                expires.map_or(approval.expires_at, |at: chrono::DateTime<Utc>| {
                    at.min(approval.expires_at)
                }),
            );
        }
        Ok(ParkedRun {
            schema_version: 1,
            session_id: self.agent_config.session_id.clone(),
            run_id: record.run_id.to_string(),
            parked_at: Utc::now().to_rfc3339(),
            expires_at: expires.ok_or("empty parked approval set")?.to_rfc3339(),
            query: record.input.to_string(),
            chat_history: Vec::new(),
            coordinator_conversation: Vec::new(),
            routing_decision: None,
            iteration: 1,
            planning_ms: 0,
            failure_history: Vec::new(),
            executed: Vec::new(),
            config_fingerprint: record.fingerprint.clone(),
            plan: ParkedPlan {
                goal: record.input.to_string(),
                steps: None,
                tasks: vec![ParkedTaskNode {
                    task_id: record.stage,
                    description: stage.id.clone(),
                    dependencies: Vec::new(),
                    worker: Some(stage.worker.clone()),
                    rationale: String::new(),
                    status: TaskStatus::AwaitingApproval,
                    result: None,
                    error: None,
                    failure_category: None,
                    attempt: Some(attempt),
                    pending: Some(pending),
                    history: snapshot.as_ref().map(|s| s.history.clone()),
                    current_prompt: snapshot.map(|s| s.current_prompt),
                }],
            },
        })
    }
}
