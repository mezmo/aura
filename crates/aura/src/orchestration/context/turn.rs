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
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(rationale: &str) -> Result<Self, ContextError> {
        todo!()
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
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(response: &str) -> Result<Self, ContextError> {
        todo!()
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
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(question: &str) -> Result<Self, ContextError> {
        todo!()
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
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(assignments: Vec<Option<WorkerRole>>) -> Result<Self, ContextError> {
        todo!()
    }

    /// Per-task worker assignments, in plan order.
    pub fn assignments(&self) -> &[Option<WorkerRole>] {
        &self.assignments
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
    pub fn render(&self) -> RenderedContext {
        todo!()
    }
}
