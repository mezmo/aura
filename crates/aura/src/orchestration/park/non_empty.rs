//! A vector that provably holds at least one element.

use serde::{Deserialize, Serialize};

/// Constrained non-empty vector: validated at construction, so a consumer
/// never re-checks emptiness (parse-don't-validate). The constructor is
/// implemented rather than staged - fixtures and serde need it live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<T>", into = "Vec<T>")]
pub struct NonEmpty<T: Clone>(Vec<T>);

/// Rejected construction of a [`NonEmpty`] from an empty vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyNonEmpty;

impl std::fmt::Display for EmptyNonEmpty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("expected at least one element")
    }
}

impl std::error::Error for EmptyNonEmpty {}

impl<T: Clone> NonEmpty<T> {
    pub fn new(items: Vec<T>) -> Result<Self, EmptyNonEmpty> {
        if items.is_empty() {
            Err(EmptyNonEmpty)
        } else {
            Ok(Self(items))
        }
    }

    pub fn first(&self) -> &T {
        &self.0[0]
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn contains(&self, item: &T) -> bool
    where
        T: PartialEq,
    {
        self.0.contains(item)
    }
}

impl<T: Clone> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = EmptyNonEmpty;

    fn try_from(items: Vec<T>) -> Result<Self, Self::Error> {
        Self::new(items)
    }
}

impl<T: Clone> From<NonEmpty<T>> for Vec<T> {
    fn from(value: NonEmpty<T>) -> Self {
        value.0
    }
}
