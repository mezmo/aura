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
///
/// Serde round-trips through the parsing constructor (`try_from = String`),
/// so a persisted goal cannot re-enter the module unvalidated.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
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
    pub fn new(original_query: &str) -> Result<Self, ContextError> {
        if original_query.trim().is_empty() {
            return Err(ContextError::EmptyGoal);
        }
        Ok(Self(original_query.to_owned()))
    }

    /// The pinned query text, verbatim.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PinnedGoal {
    type Error = ContextError;

    fn try_from(query: String) -> Result<Self, Self::Error> {
        Self::new(&query)
    }
}

impl From<PinnedGoal> for String {
    fn from(goal: PinnedGoal) -> Self {
        goal.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_goal_is_verbatim() {
        let query = "Run Windows 3.11 for Workgroups in a virtual machine using qemu.\n\nVNC Configuration Requirements:\n- Configure QEMU to use VNC display :1";
        let goal = PinnedGoal::new(query).expect("non-empty query pins");
        assert_eq!(goal.as_str(), query, "stored text is the query verbatim");
    }

    #[test]
    fn empty_or_whitespace_goal_is_rejected() {
        assert_eq!(PinnedGoal::new(""), Err(ContextError::EmptyGoal));
        assert_eq!(PinnedGoal::new("  \n\t"), Err(ContextError::EmptyGoal));
    }

    #[test]
    fn serde_round_trip_revalidates() {
        let goal = PinnedGoal::new("original query").expect("valid goal");
        let json = serde_json::to_string(&goal).expect("serializes");
        let back: PinnedGoal = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, goal);

        let empty: Result<PinnedGoal, _> = serde_json::from_str("\"  \"");
        assert!(empty.is_err(), "whitespace goal cannot re-enter via serde");
    }
}
