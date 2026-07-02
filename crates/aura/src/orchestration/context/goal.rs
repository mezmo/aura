//! The pinned goal line (`ARCHITECTURE.md` section 1.2).

use super::error::ContextError;

/// The verbatim original user query, pinned for the continuation goal line.
///
/// The continuation prompt renders this value on every iteration as
/// `Goal (verbatim from the original request):`, in place of the
/// coordinator's own drifting `plan.goal` (`ARCHITECTURE.md` section 1.2).
/// Construction happens once, from the query at request entry. The type has
/// no mutator, so the goal cannot be re-authored mid-request; requirements
/// embedded in the query cannot be summarized away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedGoal(String);

impl PinnedGoal {
    /// Pin the verbatim original user query.
    ///
    /// The stored text is the query exactly as received, never a paraphrase.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyGoal`] when the query is empty or
    /// whitespace-only.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(original_query: &str) -> Result<Self, ContextError> {
        todo!()
    }

    /// The pinned query text, verbatim.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
