//! Orchestration-specific SSE streaming events.
//!
//! Type definitions live in the [`aura_events`] crate, the single shared
//! source of truth for the SSE wire format (the same pattern base
//! [`crate::stream_events`] uses). This module re-exports them so existing
//! `crate::orchestration::...` paths keep working.
//!
//! # Event Types
//!
//! - `aura.orchestrator.plan_created` - Plan decomposed from user query
//! - `aura.orchestrator.task_started` - Worker began task execution
//! - `aura.orchestrator.task_completed` - Worker finished task (success/failure)
//! - `aura.orchestrator.iteration_complete` - Plan-execute-continue cycle done
//! - `aura.orchestrator.synthesizing` - Consolidating task results for coordinator decision
//! - `aura.orchestrator.tool_call_started` - Worker tool execution began
//! - `aura.orchestrator.tool_call_completed` - Worker tool execution finished
//!
//! Note about Scratchpad usage: usage is emitted as a base `aura.scratchpad_usage`
//! event (see `stream_events::AuraStreamEvent::ScratchpadUsage`) since it applies
//! to both single-agent and orchestration modes.

pub use aura_events::orchestration::{EventContext, OrchestrationStreamEvent, event_names};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::events::{RoutingMode, TaskDagNode};
    use crate::stream_events::{AgentContext, CorrelationContext};

    fn test_ctx() -> EventContext {
        EventContext::new(
            AgentContext::single_agent(),
            CorrelationContext::new("test-session", None),
        )
    }

    #[test]
    fn test_event_names() {
        let ctx = test_ctx();

        assert_eq!(
            OrchestrationStreamEvent::plan_created(
                "goal",
                Vec::from([
                    "Task 1 description".to_string(),
                    "Task 2 description".to_string(),
                    "Task 3 description".to_string(),
                ]),
                vec![],
                RoutingMode::Orchestrated,
                "test rationale",
                None,
                ctx.clone()
            )
            .event_name(),
            event_names::PLAN_CREATED
        );

        assert_eq!(
            OrchestrationStreamEvent::direct_answer("answer", "simple query", ctx.clone())
                .event_name(),
            event_names::DIRECT_ANSWER
        );

        assert_eq!(
            OrchestrationStreamEvent::clarification_needed(
                "which one?",
                None,
                "ambiguous",
                ctx.clone()
            )
            .event_name(),
            event_names::CLARIFICATION_NEEDED
        );

        assert_eq!(
            OrchestrationStreamEvent::task_started(
                0,
                "desc",
                vec![],
                "orch-id",
                "worker-id",
                ctx.clone()
            )
            .event_name(),
            event_names::TASK_STARTED
        );

        assert_eq!(
            OrchestrationStreamEvent::synthesizing(1, ctx).event_name(),
            event_names::SYNTHESIZING
        );
    }

    #[test]
    fn test_format_sse_task_started_includes_dependencies() {
        let event = OrchestrationStreamEvent::task_started(
            2,
            "Summarize root cause",
            vec![0, 1],
            "orch-1",
            "writer",
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TASK_STARTED)));
        assert!(sse.contains("\"dependencies\":[0,1]"));
        assert!(sse.contains("\"task_id\":2"));
    }

    #[test]
    fn test_old_task_started_without_dependencies_still_deserializes() {
        // Pre-#221 task_started payloads have no dependencies field.
        let old_payload = serde_json::json!({
            "task_id": 1,
            "description": "d",
            "worker_id": "w",
            "orchestrator_id": "o",
            "agent_id": "coordinator",
            "session_id": "s"
        });
        let event: OrchestrationStreamEvent =
            serde_json::from_value(old_payload).expect("old payload deserializes");
        assert!(matches!(
            event,
            OrchestrationStreamEvent::TaskStarted { ref dependencies, .. } if dependencies.is_empty()
        ));
    }

    // These wire-format assertions are the equivalence proof for the
    // local-enum -> aura-events consolidation: byte-for-byte JSON fragments
    // the server emitted before the swap must still be emitted after it.

    #[test]
    fn test_format_sse() {
        let event = OrchestrationStreamEvent::plan_created(
            "test goal",
            Vec::from([
                "Task 1 description".to_string(),
                "Task 2 description".to_string(),
            ]),
            vec![
                TaskDagNode {
                    id: 0,
                    dependencies: vec![],
                    worker: Some("sre".to_string()),
                },
                TaskDagNode {
                    id: 1,
                    dependencies: vec![0],
                    worker: None,
                },
            ],
            RoutingMode::Orchestrated,
            "test rationale",
            Some("coordinator response text".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::PLAN_CREATED)));
        assert!(sse.contains("\"goal\":\"test goal\""));
        assert!(sse.contains("\"tasks\":[\"Task 1 description\",\"Task 2 description\"]"));
        assert!(sse.contains(
            "\"dag\":[{\"id\":0,\"dependencies\":[],\"worker\":\"sre\"},{\"id\":1,\"dependencies\":[0]}]"
        ));
        assert!(sse.contains("\"routing_mode\":\"orchestrated\""));
        assert!(sse.contains("\"routing_rationale\":\"test rationale\""));
        assert!(sse.contains("\"planning_response\":\"coordinator response text\""));
    }

    #[test]
    fn test_format_sse_plan_created_routed() {
        let event = OrchestrationStreamEvent::plan_created(
            "simple math",
            Vec::from(["Calculate the mean of [10, 20, 30]".to_string()]),
            vec![TaskDagNode {
                id: 0,
                dependencies: vec![],
                worker: None,
            }],
            RoutingMode::Routed,
            "single worker",
            None,
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.contains("\"routing_mode\":\"routed\""));
        assert!(!sse.contains("planning_response"));
    }

    #[test]
    fn test_format_sse_plan_created_without_response() {
        let event = OrchestrationStreamEvent::plan_created(
            "goal",
            Vec::from(["Task 1".to_string()]),
            vec![],
            RoutingMode::Routed,
            "rationale",
            None,
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(!sse.contains("planning_response"));
    }

    #[test]
    fn test_format_sse_iteration_complete() {
        let event = OrchestrationStreamEvent::iteration_complete(
            1,
            false,
            Some("Single-task plan completed successfully".to_string()),
            vec![],
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.contains("\"will_replan\":false"));
        assert!(sse.contains("\"iteration\":1"));
        // Evaluation-era fields pruned during consolidation must not reappear.
        assert!(!sse.contains("quality_score"));
        assert!(!sse.contains("evaluation_skipped"));
    }

    #[test]
    fn test_format_sse_task_completed_with_result() {
        let event = OrchestrationStreamEvent::task_completed(
            0,
            true,
            1500,
            "orch-1",
            "worker-1",
            Some("The mean is 30.0".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TASK_COMPLETED)));
        assert!(sse.contains("\"result\":\"The mean is 30.0\""));
        assert!(sse.contains("\"success\":true"));
        assert!(sse.contains("\"task_id\":0"));
        assert!(sse.contains("\"orchestrator_id\":\"orch-1\""));
        assert!(sse.contains("\"worker_id\":\"worker-1\""));
    }

    #[test]
    fn test_format_sse_tool_call_started_with_arguments() {
        let args = serde_json::json!({"numbers": [10, 20, 30]});
        let event = OrchestrationStreamEvent::tool_call_started(
            Some(0),
            "call_1",
            "mean",
            "statistics",
            Some(args),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_CALL_STARTED)));
        assert!(sse.contains("\"arguments\":{\"numbers\":[10,20,30]}"));
    }

    #[test]
    fn test_format_sse_tool_call_completed_with_result() {
        let event = OrchestrationStreamEvent::tool_call_completed(
            Some(0),
            "call_1",
            true,
            42,
            Some("30.0".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_CALL_COMPLETED)));
        assert!(sse.contains("\"result\":\"30.0\""));
        assert!(sse.contains("\"success\":true"));
    }

    #[test]
    fn test_old_stream_without_tasks_field_still_deserializes() {
        // Consumer tolerance: plan_created payloads from servers older than
        // the tasks field (which carried task_count instead) must still
        // match the PlanCreated variant via #[serde(default)].
        let old_payload = serde_json::json!({
            "goal": "g",
            "task_count": 2,
            "routing_mode": "orchestrated",
            "routing_rationale": "r",
            "agent_id": "coordinator",
            "session_id": "s"
        });
        let event: OrchestrationStreamEvent =
            serde_json::from_value(old_payload).expect("old payload deserializes");
        assert!(matches!(
            event,
            OrchestrationStreamEvent::PlanCreated { ref tasks, .. } if tasks.is_empty()
        ));
    }
}
