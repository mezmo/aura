//! Background task driving the Aura agent for one investigation.

use std::collections::HashMap;
use std::sync::Arc;

use a2a::{Message, Part, Role as A2ARole, StreamResponse, TaskState};
use a2a_server::{AgentExecutor, ExecutorContext};
use futures_util::StreamExt;
use tracing::{Level, event};

use crate::investigation::client::UpdateInvestigationRequest;
use crate::investigation::finalize::{FinalizeArguments, parse_finalize_tail};
use crate::types::AppState;
use crate::{
    a2a::{AuraAgentExecutor, SharedTaskStore},
    investigation::client::InvestigationState,
};

/// Inputs the spawned task needs.
pub struct InvestigationRunnerContext {
    pub investigation_id: String,
    pub source: String,
    pub error_text: String,
    pub auth_headers: HashMap<String, String>,
    pub task_store: SharedTaskStore,
}

/// Decide the final PATCH payload based on what the agent produced.
///
/// - `finalize_arguments` is `Some` when the agent emitted a parseable JSON-tail conclusion.
/// - `terminal_state` is the last status the A2A stream emitted.
/// - `last_text` and `failure_message` are fallback sources of human-readable context for the no-finalize branch.
pub fn decide_update(
    terminal_state: Option<TaskState>,
    finalize_arguments: Option<FinalizeArguments>,
    last_text: Option<String>,
    failure_message: Option<String>,
) -> UpdateInvestigationRequest {
    let _ = terminal_state; // TODO where do we patch the A2a's store?

    match finalize_arguments {
        Some(arguments) => UpdateInvestigationRequest {
            state: Some(InvestigationState::Completed),
            confidence_score: Some(arguments.confidence_score),
            suggested_resolution: Some(arguments.suggested_resolution),
            resolution_status: Some(arguments.resolution_status),
        },
        None => {
            let resolution_text = failure_message.or(last_text).unwrap_or_else(|| {
                "Investigation ended without producing a structured conclusion.".into()
            });

            UpdateInvestigationRequest {
                state: Some(InvestigationState::Completed),
                confidence_score: None,
                suggested_resolution: Some(resolution_text),
                resolution_status: Some("failed".into()),
            }
        }
    }
}

/// Build the prompt text the agent will receive as the user message of the A2A task.
fn build_prompt(source: &str, error_text: &str) -> String {
    format!(
        "An alert requires investigation.\n\
         Source: {}\n\
         Evidence:\n\
         {}\n\
         \n\
         Investigate the issue using your available tools. When you are confident in your \
         conclusions, end your response with EXACTLY one fenced JSON block of the following \
         form, and emit nothing after the closing fence:\n\
         \n\
         ```json\n\
         {{\n  \
         \"confidence_score\": <number between 0.0 and 1.0>,\n  \
         \"suggested_resolution\": \"<natural-language recommendation>\",\n  \
         \"resolution_status\": \"<short label, e.g. resolved | mitigated | needs_escalation | no_action_required>\"\n\
         }}\n\
         ```",
        source, error_text
    )
}

/// Drive one investigation to completion/failure and report to ai-history-service
/// with the outcome.
pub async fn run_investigation(state: Arc<AppState>, context: InvestigationRunnerContext) {
    let investigation_id = context.investigation_id;

    event!(
        Level::INFO,
        %investigation_id,
        source = %context.source,
        "Investigation runner starting"
    );

    // Mark the record as investigating before kicking off the agent.
    // Best-effort, if this fails, we can still continue and try and update the service with our final result.
    if let Err(error) = state
        .ai_history_client
        .update(
            &context.auth_headers,
            &investigation_id,
            &UpdateInvestigationRequest {
                state: Some(InvestigationState::Investigating),
                ..Default::default()
            },
        )
        .await
    {
        event!(
            Level::WARN,
            %investigation_id,
            %error,
            "Failed to notify ai-history-service"
        );
    }

    let service_params: HashMap<String, Vec<String>> = context
        .auth_headers
        .iter()
        .map(|(key, value)| (key.clone(), vec![value.clone()]))
        .collect();

    let prompt = build_prompt(&context.source, &context.error_text);

    let executor_context = ExecutorContext {
        message: Some(Message::new(A2ARole::User, vec![Part::text(prompt)])),
        task_id: investigation_id.clone(),
        stored_task: None,
        context_id: investigation_id.clone(),
        metadata: None,
        user: None,
        service_params,
        tenant: None,
    };

    let executor = AuraAgentExecutor::new(state.clone(), context.task_store.clone());
    let mut stream = executor.execute(executor_context);

    let mut last_text: Option<String> = None;
    let mut terminal_state: Option<TaskState> = None;
    let mut failure_message: Option<String> = None;

    // We deliberately only react to "start & end": the terminal status and the
    // final response artifact. Intermediate events (Working pings, in-progress
    // messages, chunked/appended artifacts) are drained but ignored, because the
    // ai-history-service only cares about the final outcome of the investigation.
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamResponse::StatusUpdate(update_event)) => match update_event.status.state {
                TaskState::Working => {}
                state @ (TaskState::Completed | TaskState::Canceled | TaskState::Failed) => {
                    terminal_state = Some(state);

                    if let Some(message) = update_event.status.message {
                        let collected = message
                            .parts
                            .iter()
                            .filter_map(|p| {
                                // TODO once the A2a agent emits JSON we need to patch this
                                match &p.content {
                                    a2a::PartContent::Text(t) => Some(t.as_str()),
                                    _ => None,
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        if !collected.is_empty() {
                            failure_message = Some(collected);
                        }
                    }

                    break;
                }
                _ => {}
            },
            Ok(StreamResponse::ArtifactUpdate(update_event)) => {
                if update_event.artifact.artifact_id == "response"
                    || update_event.artifact.artifact_id == "final"
                {
                    let text = update_event
                        .artifact
                        .parts
                        .iter()
                        .filter_map(|p| {
                            // TODO once the A2a agent emits JSON we need to patch this
                            match &p.content {
                                a2a::PartContent::Text(t) => Some(t.as_str()),
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("");

                    if !text.is_empty() {
                        last_text = Some(text);
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                event!(
                    Level::ERROR,
                    investigation_id = %investigation_id,
                    error = %error,
                    "A2A stream error"
                );

                failure_message = Some(error.to_string());
                terminal_state = Some(TaskState::Failed);

                break;
            }
        }
    }

    // Parse a structured tail from the final assistant text. As part of prompt we instruct a certain return format.
    let (finalize_arguments, cleaned_text) =
        match last_text.as_deref().and_then(parse_finalize_tail) {
            Some((arguments, stripped)) => {
                (Some(arguments), (!stripped.is_empty()).then_some(stripped))
            }
            None => (None, last_text),
        };

    let update = decide_update(
        terminal_state,
        finalize_arguments,
        cleaned_text,
        failure_message,
    );

    if let Err(error) = state
        .ai_history_client
        .update(&context.auth_headers, &investigation_id, &update)
        .await
    {
        event!(
            Level::ERROR,
            %investigation_id,
            %error,
            "Failed to PATCH final investigation state"
        );
    } else {
        event!(
            Level::INFO,
            %investigation_id,
            "Investigation runner finished"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::investigation::client::InvestigationState;

    use super::{FinalizeArguments, TaskState, build_prompt, decide_update};

    #[test]
    fn prompt_includes_source_and_evidence() {
        let prompt = build_prompt("pipeline", "Service XXXX returning 400s");

        assert!(prompt.contains("Source: pipeline"));
        assert!(prompt.contains("Service XXXX returning 400s"));
        assert!(prompt.contains("```json"));
        assert!(prompt.contains("confidence_score"));
    }

    fn good_finalize() -> FinalizeArguments {
        FinalizeArguments {
            confidence_score: 0.9,
            suggested_resolution: "Roll back v42".to_owned(),
            resolution_status: "mitigated".to_owned(),
        }
    }

    #[test]
    fn finalize_arguments_carry_through_on_completed() {
        let update = decide_update(
            Some(TaskState::Completed),
            Some(good_finalize()),
            None,
            None,
        );
        assert_eq!(update.state, Some(InvestigationState::Completed));
        assert_eq!(update.confidence_score, Some(0.9));
        assert_eq!(
            update.suggested_resolution.as_deref(),
            Some("Roll back v42")
        );
        assert_eq!(update.resolution_status.as_deref(), Some("mitigated"));
    }

    #[test]
    fn finalize_arguments_carry_through_even_on_failed_stream() {
        // Tail parsed successfully but stream terminated abnormally — still trust the conclusions.
        let update = decide_update(
            Some(TaskState::Failed),
            Some(good_finalize()),
            Some("partial text".into()),
            Some("stream error".into()),
        );
        assert_eq!(update.state, Some(InvestigationState::Completed));
        assert_eq!(update.resolution_status.as_deref(), Some("mitigated"));
    }

    #[test]
    fn no_finalize_falls_back_to_failure_message() {
        let update = decide_update(
            Some(TaskState::Failed),
            None,
            Some("last assistant text".into()),
            Some("agent error: model returned 429".into()),
        );
        assert_eq!(update.state, Some(InvestigationState::Completed));
        assert_eq!(update.resolution_status.as_deref(), Some("failed"));
        assert_eq!(
            update.suggested_resolution.as_deref(),
            Some("agent error: model returned 429")
        );
        assert!(update.confidence_score.is_none());
    }

    #[test]
    fn no_finalize_falls_back_to_last_text_when_no_failure() {
        let udpate = decide_update(
            Some(TaskState::Completed),
            None,
            Some("here's what I found...".into()),
            None,
        );
        assert_eq!(udpate.resolution_status.as_deref(), Some("failed"));
        assert_eq!(
            udpate.suggested_resolution.as_deref(),
            Some("here's what I found...")
        );
    }

    #[test]
    fn no_finalize_and_no_text_uses_default() {
        let update = decide_update(None, None, None, None);
        assert_eq!(update.resolution_status.as_deref(), Some("failed"));
        assert!(
            update
                .suggested_resolution
                .as_deref()
                .unwrap()
                .contains("structured conclusion")
        );
    }
}
