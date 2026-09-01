//! Orchestration SSE events, defined in [`aura_events::orchestration`].

pub use aura_events::orchestration::{EventContext, OrchestrationStreamEvent, event_names};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_events::{AgentContext, CorrelationContext};
    use aura_events::orchestration::RoutingMode;

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
            OrchestrationStreamEvent::task_started(0, "desc", "orch-id", "worker-id", ctx.clone())
                .event_name(),
            event_names::TASK_STARTED
        );

        assert_eq!(
            OrchestrationStreamEvent::synthesizing(1, ctx).event_name(),
            event_names::SYNTHESIZING
        );
    }

    #[test]
    fn test_format_sse() {
        let event = OrchestrationStreamEvent::plan_created(
            "test goal",
            Vec::from([
                "Task 1 description".to_string(),
                "Task 2 description".to_string(),
            ]),
            RoutingMode::Orchestrated,
            "test rationale",
            Some("coordinator response text".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::PLAN_CREATED)));
        assert!(sse.contains("\"goal\":\"test goal\""));
        assert!(sse.contains("\"tasks\":[\"Task 1 description\",\"Task 2 description\"]"));
        assert!(sse.contains("\"routing_mode\":\"orchestrated\""));
        assert!(sse.contains("\"routing_rationale\":\"test rationale\""));
        assert!(sse.contains("\"planning_response\":\"coordinator response text\""));
    }

    #[test]
    fn test_format_sse_plan_created_routed() {
        let event = OrchestrationStreamEvent::plan_created(
            "simple math",
            Vec::from(["Calculate the mean of [10, 20, 30]".to_string()]),
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
            crate::orchestration::types::IterationTimings {
                planning_ms: 1200,
                execution_ms: 4500,
                task_compute_ms: 4500,
                tool_ms: 800,
            },
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.contains("\"will_replan\":false"));
        assert!(sse.contains("\"iteration\":1"));
        assert!(sse.contains("\"planning_ms\":1200"));
        assert!(sse.contains("\"execution_ms\":4500"));
        assert!(sse.contains("\"tool_ms\":800"));
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
}
