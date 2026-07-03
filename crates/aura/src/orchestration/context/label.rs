//! Correlation labels: task identity, worker role, and worker claim.
//!
//! Per-task entries in the continuation prompt and the worker prior-work
//! frame identify a task by correlation labels only — task id and worker
//! role — never by replaying the coordinator's task-description text
//! (`ARCHITECTURE.md` sections 1.3 and 3.3).

use std::num::NonZeroUsize;

use super::error::ContextError;
use crate::orchestration::tools::submit_result::Confidence;
use crate::orchestration::types::StructuredTaskOutput;

/// Identity of a task within a plan, used to correlate an evidence entry
/// with its dispatch.
///
/// The newtype keeps task identity from being confused with other counters;
/// any plan-assigned id is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(usize);

impl TaskId {
    /// Wrap a plan-assigned task id.
    pub fn new(id: usize) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A configured worker's role name, for example `operator` or `verifier`.
///
/// The role is a correlation label: it says who produced the evidence and
/// carries no instruction a worker could re-execute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerRole(String);

impl WorkerRole {
    /// Parse a worker role name.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyWorkerRole`] when the name is empty or
    /// whitespace-only.
    pub fn new(name: &str) -> Result<Self, ContextError> {
        if name.trim().is_empty() {
            return Err(ContextError::EmptyWorkerRole);
        }
        Ok(Self(name.to_owned()))
    }

    /// The role name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The correlation label for one per-task entry: task id plus worker role.
///
/// This is everything the coordinator needs to correlate an entry with its
/// dispatch (`ARCHITECTURE.md` section 1.3). There is no description field:
/// what the task did is carried by the worker's own evidence, so no
/// coordinator instruction text can sit next to that evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelationLabel {
    /// Which task this entry correlates with.
    pub task: TaskId,
    /// Worker the task was dispatched to; `None` when the plan left the
    /// task unassigned.
    pub worker: Option<WorkerRole>,
}

/// A 1-indexed orchestration iteration number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IterationNumber(NonZeroUsize);

impl IterationNumber {
    /// Parse a 1-indexed iteration number.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::ZeroIterationNumber`] for zero.
    pub fn new(iteration: usize) -> Result<Self, ContextError> {
        NonZeroUsize::new(iteration)
            .map(Self)
            .ok_or(ContextError::ZeroIterationNumber)
    }
}

impl std::fmt::Display for IterationNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A worker's own `submit_result` claim: distilled summary plus stated
/// confidence.
///
/// The pair travels together. Confidence without a summary is
/// unrepresentable, mirroring the collapse already encoded by
/// `StructuredTaskOutput` in `orchestration::types`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerClaim {
    summary: String,
    confidence: Confidence,
}

impl WorkerClaim {
    /// Parse a worker claim from its `submit_result` fields.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyWorkerClaimSummary`] when the summary is
    /// empty or whitespace-only.
    pub fn new(summary: &str, confidence: Confidence) -> Result<Self, ContextError> {
        if summary.trim().is_empty() {
            return Err(ContextError::EmptyWorkerClaimSummary);
        }
        Ok(Self {
            summary: summary.to_owned(),
            confidence,
        })
    }

    /// The worker's distilled summary of its own result.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// The worker's stated confidence.
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }
}

/// Parse a claim from a worker's structured `submit_result` output, the
/// canonical source of summary-plus-confidence pairs. Fails with
/// [`ContextError::EmptyWorkerClaimSummary`] when the summary is empty or
/// whitespace-only.
impl TryFrom<&StructuredTaskOutput> for WorkerClaim {
    type Error = ContextError;

    fn try_from(output: &StructuredTaskOutput) -> Result<Self, Self::Error> {
        Self::new(&output.summary, output.confidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_worker_role_is_rejected() {
        assert_eq!(WorkerRole::new(""), Err(ContextError::EmptyWorkerRole));
        assert_eq!(WorkerRole::new(" \t"), Err(ContextError::EmptyWorkerRole));
        assert_eq!(
            WorkerRole::new("operator").expect("valid role").as_str(),
            "operator"
        );
    }

    #[test]
    fn iteration_numbers_are_one_indexed() {
        assert_eq!(
            IterationNumber::new(0),
            Err(ContextError::ZeroIterationNumber)
        );
        let one = IterationNumber::new(1).expect("iteration 1 is valid");
        assert_eq!(one.to_string(), "1");
    }

    #[test]
    fn worker_claim_requires_summary() {
        assert_eq!(
            WorkerClaim::new("", Confidence::High),
            Err(ContextError::EmptyWorkerClaimSummary)
        );

        let empty_output = StructuredTaskOutput {
            summary: "  ".into(),
            confidence: Confidence::Low,
        };
        assert_eq!(
            WorkerClaim::try_from(&empty_output),
            Err(ContextError::EmptyWorkerClaimSummary)
        );

        let output = StructuredTaskOutput {
            summary: "Found 47 error groups".into(),
            confidence: Confidence::High,
        };
        let claim = WorkerClaim::try_from(&output).expect("summary present");
        assert_eq!(claim.summary(), "Found 47 error groups");
        assert_eq!(claim.confidence(), Confidence::High);
    }
}
