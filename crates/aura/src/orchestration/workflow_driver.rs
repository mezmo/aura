//! Configured execution uses the same worker registry and approval continuation
//! as model-planned orchestration. No coordinator plans or rewrites these stages.

use std::collections::BTreeMap;

use aura_config::workflow::compile_schema;
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Orchestrator;

use crate::orchestration::park::{ParkedRun, config_fingerprint};

use crate::orchestration::workflow::{RunRecord, RunState, RunStore, WorkflowCommand};
use crate::provider_agent::{StreamError, StreamItem};

#[path = "workflow_stages.rs"]
mod stages;

type Events = tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>;

enum StageResult {
    Complete(Value),
    Parked(Box<ParkedRun>),
    Inconclusive,
}

impl Orchestrator {
    /// Apply a transport-level control without asking a model to interpret it.
    pub async fn control_workflow(&self) -> Result<Value, StreamError> {
        if self
            .agent_config
            .workflow_request
            .as_ref()
            .is_none_or(|r| r.command == WorkflowCommand::Start)
        {
            return Err("workflow control requires an explicit non-start command".into());
        }
        let (events, _receiver) = tokio::sync::mpsc::channel(1);
        serde_json::from_str(&self.run_workflow("", &events).await?).map_err(Into::into)
    }

    pub(super) async fn run_workflow(
        &self,
        query: &str,
        events: &Events,
    ) -> Result<String, StreamError> {
        self.config.validate_stages()?;
        let root = self
            .agent_config
            .effective_memory_dir()
            .ok_or("configured workflows require memory_dir")?;
        let id = self.persistence.lock().await.run_id().parse()?;
        let scope = json!([self.agent_config.agent.name, self.agent_config.session_id]).to_string();
        let request = self.agent_config.workflow_request.as_ref();
        let command = request.map(|r| r.command).unwrap_or(WorkflowCommand::Start);
        let fingerprint = hex::encode(Sha256::digest(format!(
            "{}:{}",
            config_fingerprint(&self.agent_config),
            serde_json::to_string(&json!([
                self.config
                    .stages
                    .iter()
                    .map(|stage| (&stage.id, stage))
                    .collect::<Vec<_>>(),
                self.agent_config
                    .workflow_target_fingerprint
                    .clone()
                    .unwrap_or_else(|| crate::orchestration::workflow::target_fingerprint(
                        &self.agent_config.mcp
                    ))
            ]))?
        )));

        if command == WorkflowCommand::Inspect {
            let record = RunStore::reader(root, &scope, id)
                .load()
                .await?
                .ok_or("workflow run not found")?;
            return Ok(serde_json::to_string(&record)?);
        }
        let cancel_id = format!(
            "workflow:{}:{id}",
            hex::encode(Sha256::digest(format!("{root}:{scope}")))
        );
        if matches!(command, WorkflowCommand::Takeover | WorkflowCommand::Cancel) {
            let existing = RunStore::reader(root, &scope, id)
                .load()
                .await?
                .ok_or("workflow run not found")?;
            if let Some(request) = request
                && let Some(previous) = existing.messages.get(&request.message_id)
            {
                let digest = hex::encode(Sha256::digest(format!(
                    "{}:{query}",
                    serde_json::to_string(request)?
                )));
                if previous != &digest {
                    return Err("message id reused with different workflow content".into());
                }
                return Ok(serde_json::to_string(&existing)?);
            }
            if command != WorkflowCommand::Cancel && existing.fingerprint != fingerprint {
                return Err("workflow configuration changed".into());
            }

            crate::request_cancellation::RequestCancellation::cancel(
                &cancel_id,
                "workflow human control",
            );
        }
        let store = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match RunStore::open(root, &scope, id).await {
                    Ok(store) => break Ok::<_, StreamError>(store),
                    Err(error)
                        if matches!(
                            command,
                            WorkflowCommand::Takeover | WorkflowCommand::Cancel
                        ) && error.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await
                    }
                    Err(error) => break Err(error.into()),
                }
            }
        })
        .await
        .map_err(|_| "workflow is busy; control did not acquire execution ownership")??;
        let cancellation = crate::orchestration::workflow::RunCancellation(
            crate::request_cancellation::RequestCancellation::register(cancel_id),
        );

        let mut record = match store.load().await? {
            Some(record) => record,
            None if command == WorkflowCommand::Start => RunRecord {
                version: 1,
                run_id: id,
                agent: self.agent_config.agent.name.clone(),
                fingerprint: fingerprint.clone(),
                input: serde_json::from_str(query).unwrap_or_else(|_| Value::String(query.into())),
                stage: 0,
                results: BTreeMap::new(),
                receipts: Vec::new(),
                notifications: Vec::new(),
                pending_notification: None,
                state: RunState::Ready,
                events: Vec::new(),
                messages: BTreeMap::new(),
                verification_deadline: None,
            },
            None => return Err("workflow run not found in this agent/session".into()),
        };
        if command == WorkflowCommand::Inspect {
            return Ok(serde_json::to_string(&record)?);
        }
        if let Some(request) = request {
            let digest = hex::encode(Sha256::digest(format!(
                "{}:{query}",
                serde_json::to_string(request)?
            )));
            if let Some(previous) = record.messages.get(&request.message_id) {
                if previous != &digest {
                    return Err("message id reused with different workflow content".into());
                }
                return Ok(serde_json::to_string(&record)?);
            }
            record.messages.insert(request.message_id.clone(), digest);
        }
        if matches!(record.state, RunState::Running) {
            record.transition(
                RunState::Uncertain,
                "execution interrupted; reconcile externally before starting a replacement run",
            );
            save_workflow(&record, &store, events).await?;
        }
        match command {
            WorkflowCommand::Cancel => record.transition(
                RunState::Cancelled {
                    execution_uncertain: record.state.execution_uncertain(),
                },
                "cancelled by human",
            ),
            WorkflowCommand::Takeover if !matches!(record.state, RunState::HumanOwned { .. }) => {
                record.transition(
                    RunState::HumanOwned {
                        suspended: Box::new(record.state.clone()),
                    },
                    "human takeover",
                )
            }
            WorkflowCommand::Resume => {
                if let RunState::HumanOwned { suspended } = &record.state {
                    record.transition((**suspended).clone(), "human returned execution control");
                }
            }

            _ => {}
        }
        if let Some(notification) = record.pending_notification.clone() {
            match &record.state {
                RunState::Received { output } => {
                    if let Err(error) = decode_receipt(output) {
                        record.pending_notification = None;
                        record.transition(
                            RunState::Failed {
                                reason: error.to_string(),
                            },
                            "notification receipt failed",
                        );
                    } else {
                        record.notifications.push(notification.key);
                        record.pending_notification = None;
                        record
                            .transition(*notification.suspended, "notification receipt recovered");
                    }
                    save_workflow(&record, &store, events).await?;
                }
                RunState::AwaitingApproval { .. } => record.pending_notification = None,
                _ => {}
            }
        }
        if record.fingerprint != fingerprint && !matches!(command, WorkflowCommand::Cancel) {
            return Err("workflow configuration changed; refusing to resume".into());
        }
        if matches!(
            record.state,
            RunState::Cancelled { .. }
                | RunState::HumanOwned { .. }
                | RunState::Uncertain
                | RunState::Completed
                | RunState::Failed { .. }
                | RunState::Inconclusive
        ) {
            save_workflow(&record, &store, events).await?;
            if matches!(record.state, RunState::Cancelled { .. })
                && let Ok(registry) = self.workflow_registry()
            {
                registry
                    .cancel_request(&format!("run:{}", record.run_id))
                    .await;
            }
            return Ok(serde_json::to_string(&record)?);
        }
        if command == WorkflowCommand::Start && record.stage != 0 {
            return Err("existing workflow requires resume".into());
        }
        self.validate_workflow_tools()?;
        save_workflow(&record, &store, events).await?;
        while let Some(stage) = self.config.stages.get(record.stage) {
            let result = tokio::select! {
                result = self.run_stage(stage, &mut record, &store, events) => result,
                _ = self.workflow_cancel.cancelled() => {
                    record.transition(RunState::Cancelled { execution_uncertain: record.state.execution_uncertain() }, "request cancelled");
                    save_workflow(&record, &store, events).await?;
                    return Ok(serde_json::to_string(&record)?);
                }
                _ = cancellation.0.token.cancelled() => {

                    if matches!(record.state, RunState::Running) {
                        record.transition(RunState::Uncertain, "interrupted invocation may have executed");
                    }
                    save_workflow(&record, &store, events).await?;
                    return Ok(serde_json::to_string(&record)?);
                }
            };

            match result {
                Ok(StageResult::Complete(value)) => {
                    if let Err(error) = compile_schema(&stage.output_schema)?.validate(&value) {
                        record.transition(
                            RunState::Failed {
                                reason: format!("invalid stage handoff: {error}"),
                            },
                            "handoff rejected",
                        );
                        save_workflow(&record, &store, events).await?;
                        break;
                    }
                    record.results.insert(stage.id.clone(), value);
                    record.stage += 1;
                    record.verification_deadline = None;
                    record.transition(RunState::Ready, "stage completed");
                }
                Ok(StageResult::Parked(checkpoint)) => {
                    if !matches!(record.state, RunState::AwaitingApproval { .. }) {
                        record.transition(
                            RunState::AwaitingApproval { checkpoint },
                            "awaiting approval",
                        );
                        save_workflow(&record, &store, events).await?;
                        if let Some(guard) = &self.park_guard {
                            guard.mark_published();
                        }
                    }
                    tokio::select! {
                            result = self.workflow_notifications(stage, &mut record, &store) => result?,
                            _ = self.workflow_cancel.cancelled() => {
                        record.transition(RunState::Cancelled { execution_uncertain: record.state.execution_uncertain() }, "request cancelled");
                        save_workflow(&record, &store, events).await?;
                        return Ok(serde_json::to_string(&record)?);
                    }
                    _ = cancellation.0.token.cancelled() => {

                                if matches!(record.state, RunState::Running) { record.transition(RunState::Uncertain, "notification interrupted"); }
                                save_workflow(&record, &store, events).await?;
                                return Ok(serde_json::to_string(&record)?);
                            }
                        }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                        _ = self.workflow_cancel.cancelled() => {
                            record.transition(RunState::Cancelled { execution_uncertain: record.state.execution_uncertain() }, "request cancelled");
                            save_workflow(&record, &store, events).await?;
                            return Ok(serde_json::to_string(&record)?);
                        },
                        _ = cancellation.0.token.cancelled() => return Ok(serde_json::to_string(&record)?),
                    }
                    continue;
                }
                Ok(StageResult::Inconclusive) => {
                    record.transition(
                        RunState::Inconclusive,
                        "verification deadline reached without recovery evidence",
                    );
                    save_workflow(&record, &store, events).await?;
                    break;
                }
                Err(error) => {
                    if matches!(record.state, RunState::Running) {
                        record.transition(
                            RunState::Uncertain,
                            &format!("execution outcome uncertain: {error}"),
                        );
                    } else {
                        record.transition(
                            RunState::Failed {
                                reason: error.to_string(),
                            },
                            "stage failed",
                        );
                    }

                    save_workflow(&record, &store, events).await?;
                    break;
                }
            }
            save_workflow(&record, &store, events).await?;
        }
        if record.stage == self.config.stages.len() {
            record.transition(RunState::Completed, "workflow completed");
            save_workflow(&record, &store, events).await?;
        }
        Ok(serde_json::to_string(&record)?)
    }
}

fn deadline(seconds: u64) -> Result<chrono::DateTime<Utc>, StreamError> {
    let duration = chrono::Duration::seconds(i64::try_from(seconds)?);
    Utc::now()
        .checked_add_signed(duration)
        .ok_or_else(|| "workflow deadline out of range".into())
}

fn merge_arguments(
    arguments: &Value,
    inputs: serde_json::Map<String, Value>,
) -> Result<Value, StreamError> {
    let mut arguments = arguments
        .as_object()
        .ok_or("tool arguments must be an object")?
        .clone();
    for (key, value) in inputs {
        if arguments.insert(key.clone(), value).is_some() {
            return Err(format!("input {key} overwrites a configured argument").into());
        }
    }
    Ok(Value::Object(arguments))
}

fn decode_receipt(output: &str) -> Result<Value, StreamError> {
    let value: Value = serde_json::from_str(output)?;
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
    Ok(value)
}

async fn save_workflow(
    record: &RunRecord,
    store: &RunStore,
    events: &Events,
) -> Result<(), StreamError> {
    store.save(record).await?;
    let event = crate::orchestration::OrchestratorEvent::WorkflowUpdated {
        run: serde_json::to_value(record)?,
    };
    // Durable state must never depend on an attached consumer draining a stream.
    let _ = events.try_send(Ok(StreamItem::OrchestratorEvent(event)));
    Ok(())
}

#[cfg(test)]
#[path = "workflow_driver_tests.rs"]
mod tests;
