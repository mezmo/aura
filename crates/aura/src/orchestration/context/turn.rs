//! Compact coordinator decision turns (`ARCHITECTURE.md` section 2.3).
//!
//! After every routing decision, one assistant turn is recorded in the
//! coordinator conversation. These types replace the pretty-printed
//! `PlanningResponse` JSON with a compact record that has no field able to
//! hold a task body, which removes the history side of the double-render
//! problem (`ARCHITECTURE.md` section 2.2).

use super::error::ContextError;
use super::label::WorkerRole;
use super::rendered::RenderedContext;
use crate::orchestration::types::{PlanningResponse, StepInput};

/// The coordinator's stated reason for a routing choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingRationale(String);

impl RoutingRationale {
    /// Parse a routing rationale.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyRoutingRationale`] when the rationale is
    /// empty or whitespace-only.
    pub fn new(rationale: &str) -> Result<Self, ContextError> {
        if rationale.trim().is_empty() {
            return Err(ContextError::EmptyRoutingRationale);
        }
        Ok(Self(rationale.to_owned()))
    }

    /// The rationale text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The final response text the model produced for a `respond_directly`
/// decision — what the model actually said, not a summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalResponse(String);

impl FinalResponse {
    /// Parse a final response.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyFinalResponse`] when the response is
    /// empty or whitespace-only.
    pub fn new(response: &str) -> Result<Self, ContextError> {
        if response.trim().is_empty() {
            return Err(ContextError::EmptyFinalResponse);
        }
        Ok(Self(response.to_owned()))
    }

    /// The response text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The question returned to the user for a `request_clarification`
/// decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarificationQuestion(String);

impl ClarificationQuestion {
    /// Parse a clarification question.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyClarificationQuestion`] when the
    /// question is empty or whitespace-only.
    pub fn new(question: &str) -> Result<Self, ContextError> {
        if question.trim().is_empty() {
            return Err(ContextError::EmptyClarificationQuestion);
        }
        Ok(Self(question.to_owned()))
    }

    /// The question text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One-line shape of a created plan: per-task worker assignments in plan
/// order.
///
/// Task count is the list length, so shape and count cannot disagree, and
/// there is no field that can hold a task body. Renders as
/// `1 task (operator)` or `3 tasks (analyst, operator, verifier)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanShape {
    assignments: Vec<Option<WorkerRole>>,
}

impl PlanShape {
    /// Parse a plan shape from per-task worker assignments; `None` marks an
    /// unassigned task.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyPlanShape`] when the list is empty.
    pub fn new(assignments: Vec<Option<WorkerRole>>) -> Result<Self, ContextError> {
        if assignments.is_empty() {
            return Err(ContextError::EmptyPlanShape);
        }
        Ok(Self { assignments })
    }

    /// Per-task worker assignments, in plan order.
    pub fn assignments(&self) -> &[Option<WorkerRole>] {
        &self.assignments
    }
}

impl std::fmt::Display for PlanShape {
    /// `1 task (operator)` / `3 tasks (analyst, operator, verifier)`; a
    /// task the plan left unassigned renders as `unassigned`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.assignments.len();
        let noun = if count == 1 { "task" } else { "tasks" };
        let workers = self
            .assignments
            .iter()
            .map(|worker| worker.as_ref().map_or("unassigned", WorkerRole::as_str))
            .collect::<Vec<_>>()
            .join(", ");
        write!(f, "{count} {noun} ({workers})")
    }
}

/// The compact assistant turn recorded in the coordinator conversation
/// after a routing decision.
///
/// For `create_plan` the turn records the variant, the routing rationale,
/// and the plan shape — never task bodies; the full plan reaches the
/// workers and the run journal instead (`ARCHITECTURE.md` section 2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorTurn {
    /// A `create_plan` decision: rationale plus plan shape, no task bodies.
    CreatePlan {
        /// Why the coordinator chose to plan.
        rationale: RoutingRationale,
        /// Task count and worker assignments.
        shape: PlanShape,
    },
    /// A `respond_directly` decision: the model's actual final response
    /// text.
    RespondDirectly {
        /// What the model said.
        response: FinalResponse,
    },
    /// A `request_clarification` decision: the question that ends the run.
    RequestClarification {
        /// The question returned to the user.
        question: ClarificationQuestion,
    },
}

impl CoordinatorTurn {
    /// Render the compact decision text recorded as the assistant turn,
    /// for example `create_plan: 1 task (operator). Rationale: ...`.
    ///
    /// For `respond_directly` and `request_clarification` the recorded turn
    /// is the text the model produced, verbatim: the final response is
    /// "what the model actually said" (`ARCHITECTURE.md` section 2.3), and
    /// the clarification turn quotes the question symmetrically
    /// (`TYPE_PLAN.md` gate decision 3).
    pub fn render(&self) -> RenderedContext {
        let text = match self {
            Self::CreatePlan { rationale, shape } => {
                format!("create_plan: {shape}. Rationale: {}", rationale.as_str())
            }
            Self::RespondDirectly { response } => response.as_str().to_owned(),
            Self::RequestClarification { question } => question.as_str().to_owned(),
        };
        RenderedContext::new(text)
    }
}

/// Parse the compact decision turn from the routing decision the model
/// wrote, the canonical boundary conversion for the recording site in
/// `plan_with_routing`.
///
/// Only decision-identifying fields cross over: the rationale and leaf
/// worker assignments for `create_plan`, the response text for
/// `respond_directly`, and the question for `request_clarification`.
/// Task bodies, the plan goal (pinned separately, `ARCHITECTURE.md`
/// section 1.2), the planning summary (a paraphrase of task work), the
/// direct-response summary, and clarification options are all dropped:
/// no [`CoordinatorTurn`] field can hold them.
impl TryFrom<&PlanningResponse> for CoordinatorTurn {
    type Error = ContextError;

    fn try_from(decision: &PlanningResponse) -> Result<Self, Self::Error> {
        match decision {
            PlanningResponse::StepsPlan {
                steps,
                routing_rationale,
                ..
            } => Ok(Self::CreatePlan {
                rationale: RoutingRationale::new(routing_rationale)?,
                shape: PlanShape::new(leaf_assignments(steps))?,
            }),
            PlanningResponse::Direct { response, .. } => Ok(Self::RespondDirectly {
                response: FinalResponse::new(response)?,
            }),
            PlanningResponse::Clarification { question, .. } => Ok(Self::RequestClarification {
                question: ClarificationQuestion::new(question)?,
            }),
        }
    }
}

/// Worker assignments of the plan's leaf tasks, in the traversal order
/// `flatten_steps` assigns task ids. A blank worker name is an unassigned
/// task, matching the correlation-label convention in the continuation
/// renderer.
fn leaf_assignments(steps: &[StepInput]) -> Vec<Option<WorkerRole>> {
    fn collect(steps: &[StepInput], assignments: &mut Vec<Option<WorkerRole>>) {
        for step in steps {
            match step {
                StepInput::LeafTask { worker, .. } => assignments.push(
                    worker
                        .as_deref()
                        .and_then(|name| WorkerRole::new(name).ok()),
                ),
                StepInput::ParallelGroup { items } => collect(items, assignments),
                StepInput::SubChain { steps } => collect(steps, assignments),
            }
        }
    }
    let mut assignments = Vec::new();
    collect(steps, &mut assignments);
    assignments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str) -> Option<WorkerRole> {
        Some(WorkerRole::new(name).expect("valid role"))
    }

    #[test]
    fn empty_turn_payloads_are_rejected() {
        assert_eq!(
            RoutingRationale::new(" \t"),
            Err(ContextError::EmptyRoutingRationale)
        );
        assert_eq!(
            FinalResponse::new(""),
            Err(ContextError::EmptyFinalResponse)
        );
        assert_eq!(
            ClarificationQuestion::new("\n"),
            Err(ContextError::EmptyClarificationQuestion)
        );
        assert_eq!(
            PlanShape::new(Vec::new()),
            Err(ContextError::EmptyPlanShape)
        );
    }

    #[test]
    fn plan_shape_renders_count_and_assignments() {
        let one = PlanShape::new(vec![role("operator")]).expect("non-empty");
        assert_eq!(one.to_string(), "1 task (operator)");

        let three =
            PlanShape::new(vec![role("analyst"), None, role("verifier")]).expect("non-empty");
        assert_eq!(three.to_string(), "3 tasks (analyst, unassigned, verifier)");
        assert_eq!(three.assignments().len(), 3);
    }

    // The `ARCHITECTURE.md` section 2.3 example, verbatim.
    #[test]
    fn create_plan_turn_renders_the_architecture_example() {
        let turn = CoordinatorTurn::CreatePlan {
            rationale: RoutingRationale::new(
                "launch the VM and expose it over VNC before verifying input.",
            )
            .expect("non-empty"),
            shape: PlanShape::new(vec![role("operator")]).expect("non-empty"),
        };
        assert_eq!(
            turn.render().as_str(),
            "create_plan: 1 task (operator). Rationale: launch the VM and expose it over VNC before verifying input."
        );
    }

    // TYPE_PLAN.md: the respond_directly turn is the model's actual final
    // response text, and the request_clarification turn quotes the
    // question, symmetric with it (gate decision 3).
    #[test]
    fn terminal_turns_record_the_model_text_verbatim() {
        let respond = CoordinatorTurn::RespondDirectly {
            response: FinalResponse::new("Kafka lag is back under 100 messages.")
                .expect("non-empty"),
        };
        assert_eq!(
            respond.render().as_str(),
            "Kafka lag is back under 100 messages."
        );

        let clarify = CoordinatorTurn::RequestClarification {
            question: ClarificationQuestion::new("Which environment should I audit?")
                .expect("non-empty"),
        };
        assert_eq!(
            clarify.render().as_str(),
            "Which environment should I audit?"
        );
    }

    #[test]
    fn decision_conversion_keeps_shape_and_drops_task_bodies() {
        let decision = PlanningResponse::StepsPlan {
            goal: "Audit ingest pipeline health".to_owned(),
            steps: vec![
                StepInput::LeafTask {
                    task: "Enumerate pods in namespace ingest".to_owned(),
                    worker: Some("operator".to_owned()),
                },
                StepInput::ParallelGroup {
                    items: vec![
                        StepInput::LeafTask {
                            task: "Sample consumer lag metrics".to_owned(),
                            worker: None,
                        },
                        StepInput::SubChain {
                            steps: vec![StepInput::LeafTask {
                                task: "Cross-check broker logs".to_owned(),
                                worker: Some("verifier".to_owned()),
                            }],
                        },
                    ],
                },
            ],
            routing_rationale: "Pipeline health needs live cluster access.".to_owned(),
            planning_summary: "Enumerate, sample, cross-check.".to_owned(),
        };

        let turn = CoordinatorTurn::try_from(&decision).expect("recordable decision");
        let rendered = turn.render();
        assert_eq!(
            rendered.as_str(),
            "create_plan: 3 tasks (operator, unassigned, verifier). \
             Rationale: Pipeline health needs live cluster access."
        );
        for task_body in [
            "Enumerate pods in namespace ingest",
            "Sample consumer lag metrics",
            "Cross-check broker logs",
        ] {
            assert!(
                !rendered.as_str().contains(task_body),
                "recorded turn must not replay the task body {task_body:?}"
            );
        }
    }

    #[test]
    fn conversion_rejects_unrecordable_decisions() {
        let no_rationale = PlanningResponse::StepsPlan {
            goal: "g".to_owned(),
            steps: vec![StepInput::LeafTask {
                task: "t".to_owned(),
                worker: None,
            }],
            routing_rationale: "  ".to_owned(),
            planning_summary: String::new(),
        };
        assert_eq!(
            CoordinatorTurn::try_from(&no_rationale),
            Err(ContextError::EmptyRoutingRationale)
        );

        let no_tasks = PlanningResponse::StepsPlan {
            goal: "g".to_owned(),
            steps: Vec::new(),
            routing_rationale: "r".to_owned(),
            planning_summary: String::new(),
        };
        assert_eq!(
            CoordinatorTurn::try_from(&no_tasks),
            Err(ContextError::EmptyPlanShape)
        );

        let no_response = PlanningResponse::Direct {
            response: String::new(),
            routing_rationale: "r".to_owned(),
            response_summary: None,
        };
        assert_eq!(
            CoordinatorTurn::try_from(&no_response),
            Err(ContextError::EmptyFinalResponse)
        );

        let no_question = PlanningResponse::Clarification {
            question: " ".to_owned(),
            options: None,
            routing_rationale: "r".to_owned(),
        };
        assert_eq!(
            CoordinatorTurn::try_from(&no_question),
            Err(ContextError::EmptyClarificationQuestion)
        );
    }
}
