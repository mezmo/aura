//! Header tables (`headers`, `headers_from_request`) parsed at load time.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::ops::Deref;

use http::header::HeaderName;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// A header table keyed by header name: every name is a valid HTTP header
/// name, stored in lowercase, and no two configured names are equal ignoring
/// case.
///
/// Header names are case-insensitive, so two spellings of one name would
/// collapse into one entry and the value that survives would depend on
/// iteration order. Parsing rejects such a pair instead.
///
/// The map is read-only through [`Deref`]. [`HeaderMap::insert`] takes a
/// [`HeaderName`], so every key stays valid and lowercase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct HeaderMap(HashMap<String, String>);

/// Why a [`HeaderMap`] could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeaderMapError {
    #[error("'{name}' is not a valid header name")]
    InvalidName { name: String },

    #[error("header names '{first}' and '{second}' differ only by case; keep one")]
    CaseCollision { first: String, second: String },
}

impl HeaderMap {
    /// An empty header table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `name` to `value`, returning the value it replaces, if any.
    pub fn insert(&mut self, name: HeaderName, value: String) -> Option<String> {
        self.0.insert(name.as_str().to_owned(), value)
    }

    /// Each entry with its name as a [`HeaderName`].
    pub fn header_names(&self) -> impl Iterator<Item = (HeaderName, &str)> {
        self.0.iter().map(|(name, value)| {
            let name = HeaderName::from_bytes(name.as_bytes())
                .expect("HeaderMap keys are parsed header names");
            (name, value.as_str())
        })
    }

    /// Mutable access to the values only; the names cannot change.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut String> {
        self.0.values_mut()
    }
}

impl Deref for HeaderMap {
    type Target = HashMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Collects entries one at a time, keeping the configured spelling of each
/// name for the collision error.
struct Builder {
    map: HashMap<String, String>,
    spellings: HashMap<String, String>,
}

impl Builder {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            spellings: HashMap::with_capacity(capacity),
        }
    }

    fn push(&mut self, name: String, value: String) -> Result<(), HeaderMapError> {
        let parsed = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HeaderMapError::InvalidName { name: name.clone() })?;
        match self.spellings.entry(parsed.as_str().to_owned()) {
            Entry::Occupied(first) => Err(HeaderMapError::CaseCollision {
                first: first.get().clone(),
                second: name,
            }),
            Entry::Vacant(slot) => {
                self.map.insert(slot.key().clone(), value);
                slot.insert(name);
                Ok(())
            }
        }
    }

    fn finish(self) -> HeaderMap {
        HeaderMap(self.map)
    }
}

impl TryFrom<HashMap<String, String>> for HeaderMap {
    type Error = HeaderMapError;

    /// Entries are checked in name order, so the error for a collision is
    /// deterministic and names the spelling that sorts second.
    fn try_from(raw: HashMap<String, String>) -> Result<Self, Self::Error> {
        let mut entries: Vec<_> = raw.into_iter().collect();
        entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        let mut builder = Builder::with_capacity(entries.len());
        for (name, value) in entries {
            builder.push(name, value)?;
        }
        Ok(builder.finish())
    }
}

impl<'de> Deserialize<'de> for HeaderMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HeaderMapVisitor;

        impl<'de> Visitor<'de> for HeaderMapVisitor {
            type Value = HeaderMap;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a table of header names to string values")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<HeaderMap, A::Error> {
                let mut builder = Builder::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((name, value)) = access.next_entry::<String, String>()? {
                    builder.push(name, value).map_err(de::Error::custom)?;
                }
                Ok(builder.finish())
            }
        }

        deserializer.deserialize_map(HeaderMapVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn names_are_stored_lowercase() {
        let map = HeaderMap::try_from(raw(&[("X-Tenant", "t"), ("Authorization", "a")])).unwrap();
        assert_eq!(map.get("x-tenant").map(String::as_str), Some("t"));
        assert_eq!(map.get("authorization").map(String::as_str), Some("a"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn case_collision_names_both_spellings() {
        let err = HeaderMap::try_from(raw(&[("authorization", "b"), ("Authorization", "a")]))
            .unwrap_err();
        assert_eq!(
            err,
            HeaderMapError::CaseCollision {
                first: "Authorization".to_owned(),
                second: "authorization".to_owned(),
            }
        );
    }

    #[test]
    fn invalid_name_is_rejected() {
        let err = HeaderMap::try_from(raw(&[("X Tenant", "t")])).unwrap_err();
        assert_eq!(
            err,
            HeaderMapError::InvalidName {
                name: "X Tenant".to_owned()
            }
        );
    }

    #[test]
    fn insert_replaces_by_lowercase_name() {
        let mut map = HeaderMap::try_from(raw(&[("Authorization", "static")])).unwrap();
        let previous = map.insert(
            HeaderName::from_static("authorization"),
            "mapped".to_owned(),
        );
        assert_eq!(previous.as_deref(), Some("static"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn deserialize_rejects_case_collision() {
        let err = toml::from_str::<HeaderMap>("X-Tenant = \"a\"\nx-tenant = \"b\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("header names 'X-Tenant' and 'x-tenant' differ only by case"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serializes_as_a_plain_table() {
        let map = HeaderMap::try_from(raw(&[("X-Tenant", "t")])).unwrap();
        assert_eq!(toml::to_string(&map).unwrap(), "x-tenant = \"t\"\n");
    }
}
