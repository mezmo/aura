//! The worker dependency-context frame (`ARCHITECTURE.md` sections
//! 3.2-3.5).
//!
//! The frame replaces `build_task_context` outright: it presents read-only
//! evidence from completed ancestor tasks, structured per prior task, under
//! a token budget. No entry has a field for the prior task's
//! coordinator-authored description, so the stale-instruction leakage of
//! the old `COMPLETED — Task {id} ({description})` format is
//! unrepresentable (`ARCHITECTURE.md` sections 3.1 and 3.5).

use std::num::NonZeroUsize;

use super::error::ContextError;
use super::evidence::EvidenceEntry;
use super::label::CorrelationLabel;
use super::rendered::RenderedContext;

/// Distance from the current task to a transitive ancestor, in dependency
/// edges. Always at least 2: distance 1 is a direct dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AncestorDistance(NonZeroUsize);

impl AncestorDistance {
    /// The smallest transitive distance.
    pub const MIN: usize = 2;

    /// Parse a transitive ancestor distance.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::TransitiveDistanceIsDirect`] for distances
    /// below [`Self::MIN`]; a distance-1 relation must use
    /// [`DependencyRelation::Direct`].
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(distance: usize) -> Result<Self, ContextError> {
        todo!()
    }
}

impl std::fmt::Display for AncestorDistance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How a prior task relates to the current task in the plan DAG, rendered
/// as `Dependency: direct` or `Dependency: transitive` so the worker can
/// weight nearer evidence (`ARCHITECTURE.md` section 3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyRelation {
    /// A direct dependency of the current task. Direct entries are the
    /// budget floor and are always kept.
    Direct,
    /// A transitive ancestor at the given distance. Transitive entries fill
    /// the remaining budget nearest-first.
    Transitive {
        /// Edge distance from the current task; at least 2.
        distance: AncestorDistance,
    },
}

/// Token budget capping the rendered worker context frame.
///
/// Semantics fixed by the architecture (`ARCHITECTURE.md` section 3.4):
/// direct dependencies are the floor and are always kept; transitive
/// ancestors fill the remaining budget nearest-first; the frame header and
/// separators count against the budget. The default is 8000 tokens to
/// match the accepted baseline binary. Whether the value becomes a config
/// knob or a constant is an R3c decision; either way this type carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget(NonZeroUsize);

impl TokenBudget {
    /// Parse a token budget.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::ZeroTokenBudget`] for zero.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(tokens: usize) -> Result<Self, ContextError> {
        todo!()
    }

    /// The budget, in tokens.
    pub fn get(&self) -> NonZeroUsize {
        self.0
    }
}

impl Default for TokenBudget {
    /// The 8000-token default the accepted baseline binary ran with
    /// (`ARCHITECTURE.md` section 3.4).
    fn default() -> Self {
        const DEFAULT_TOKENS: NonZeroUsize = match NonZeroUsize::new(8000) {
            Some(tokens) => tokens,
            None => panic!("default token budget is nonzero"),
        };
        Self(DEFAULT_TOKENS)
    }
}

/// One prior task's entry in the frame: correlation label, dependency
/// relation, and the prior worker's own evidence.
///
/// Renders as the `Prior Task {id}` block — `Worker:`, `Dependency:`,
/// `Summary:`/`Confidence:` when attested, and `Evidence:`
/// (`ARCHITECTURE.md` section 3.3). The prior task's description has no
/// field here (`ARCHITECTURE.md` section 3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorWorkEntry {
    /// Task id and worker role; no instruction text.
    pub label: CorrelationLabel,
    /// Direct dependency or transitive ancestor.
    pub relation: DependencyRelation,
    /// The prior worker's own reported evidence.
    pub evidence: EvidenceEntry,
}

impl PriorWorkEntry {
    /// Render the `Prior Task {id}` block for the frame.
    pub fn render(&self) -> RenderedContext {
        todo!()
    }
}

/// The assembled `READ-ONLY PRIOR WORK` frame for one task: completed
/// ancestor entries admitted under the token budget, rendered oldest-first
/// behind the "evidence, not instructions to replay" header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerContextFrame {
    entries: Vec<PriorWorkEntry>,
    budget: TokenBudget,
}

impl WorkerContextFrame {
    /// Assemble a frame from the candidate ancestor entries, applying the
    /// budget: direct entries are always kept; transitive entries are
    /// admitted nearest-first while the budget (charged for header,
    /// separators, and entries) holds; admitted entries keep plan order,
    /// oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyWorkerContextFrame`] when `entries` is
    /// empty. A task with no completed ancestors gets no frame at all.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: budget accounting lands with card R3c"
    )]
    pub fn assemble(
        entries: Vec<PriorWorkEntry>,
        budget: TokenBudget,
    ) -> Result<Self, ContextError> {
        todo!()
    }

    /// The admitted entries, oldest first.
    pub fn entries(&self) -> &[PriorWorkEntry] {
        &self.entries
    }

    /// The budget the frame was assembled under.
    pub fn budget(&self) -> TokenBudget {
        self.budget
    }

    /// Render the full frame, header included, for the `%%CONTEXT%%` slot
    /// of the worker task prompt.
    pub fn render(&self) -> RenderedContext {
        todo!()
    }
}
