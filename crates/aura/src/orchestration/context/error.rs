//! Error type for parsing coordinator-context values.

/// Why a context value failed to parse.
///
/// Every fallible constructor in this module returns `ContextError`, so
/// downstream code only ever holds already-valid context values. Variants
/// carry no payload: each names the rule that was violated, and the caller
/// already holds the offending input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    /// The pinned goal text was empty or whitespace-only.
    #[error("pinned goal text is empty")]
    EmptyGoal,
    /// A worker role name was empty or whitespace-only.
    #[error("worker role name is empty")]
    EmptyWorkerRole,
    /// A worker-claim summary was empty or whitespace-only.
    #[error("worker-claim summary is empty")]
    EmptyWorkerClaimSummary,
    /// Inline evidence text was empty or whitespace-only.
    #[error("inline evidence text is empty")]
    EmptyEvidenceText,
    /// Inline evidence text carried a spill footer; a spilled result must
    /// parse as an artifact-pointer entry instead.
    #[error("inline evidence text contains an artifact spill footer")]
    InlineEvidenceCarriesSpillFooter,
    /// A result preview was empty or whitespace-only.
    #[error("result preview is empty")]
    EmptyResultPreview,
    /// An artifact filename was empty or whitespace-only.
    #[error("artifact filename is empty")]
    EmptyArtifactFilename,
    /// A routing rationale was empty or whitespace-only.
    #[error("routing rationale is empty")]
    EmptyRoutingRationale,
    /// A final response text was empty or whitespace-only.
    #[error("final response text is empty")]
    EmptyFinalResponse,
    /// A clarification question was empty or whitespace-only.
    #[error("clarification question is empty")]
    EmptyClarificationQuestion,
    /// A plan shape had no tasks; `flatten_steps` rejects empty plans, so a
    /// decision turn for one is invalid.
    #[error("plan shape has no tasks")]
    EmptyPlanShape,
    /// A failure handle was derived from an empty task description.
    #[error("failure handle source description is empty")]
    EmptyFailureDescription,
    /// Iteration numbers are 1-indexed; zero is not a valid iteration.
    #[error("iteration number is zero; iterations are 1-indexed")]
    ZeroIterationNumber,
    /// A token budget of zero tokens can render nothing.
    #[error("token budget is zero")]
    ZeroTokenBudget,
    /// A transitive ancestor distance below 2 describes a direct dependency
    /// and must use `DependencyRelation::Direct`.
    #[error("transitive ancestor distance is below 2; distance 1 is a direct dependency")]
    TransitiveDistanceIsDirect,
    /// A prior-work frame had no entries; a task with no completed
    /// ancestors gets no frame at all rather than an empty one.
    #[error("prior-work frame has no entries")]
    EmptyPriorWorkFrame,
    /// A named-check identity was empty or whitespace-only.
    #[error("named-check identity is empty")]
    EmptyCheckIdentity,
    /// A named-check identity exceeded the field's character bound.
    #[error("named-check identity exceeds the field bound")]
    CheckIdentityTooLong,
    /// A named-check result was empty or whitespace-only; a check that was not
    /// run is `CheckOutcome::NotRun`, never a blank result.
    #[error("named-check result is empty")]
    EmptyCheckResult,
    /// A named-check result exceeded the field's character bound; bulk output
    /// belongs in the spilled artifact, not the decisive check line.
    #[error("named-check result exceeds the field bound")]
    CheckResultTooLong,
}
