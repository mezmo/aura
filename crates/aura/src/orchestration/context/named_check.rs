//! The two-ended named check: the verification a task's success depends on,
//! paired with what the worker's result carried for it.
//!
//! A task MAY name a check that decides its success — a specific verification
//! whose result determines pass or fail (`docs/redesign/2026-07-21-s46-enforcement-mockups.md`
//! section 2, Rule 1). When it does, completion is incomplete until the
//! worker's result carries that check and the result it produced. This module
//! is the type home for that decisive datum: [`NamedCheck`] carries the check
//! identity plus its outcome, bounded so it stays a single decisive line that
//! survives result spill (packet section 7) rather than growing into the bulk
//! transcript.
//!
//! # Design status
//!
//! S46 phase 1 landed this module as a type skeleton. The parsing
//! constructors ([`CheckIdentity::new`], [`CheckResult::new`], [`NamedCheck::parse`])
//! are implemented; the render leg ([`NamedCheck::render_line`]) is a
//! `todo!()` body for phase 2, along with the reconciliation that turns a
//! declared-but-absent check into [`CheckOutcome::NotRun`] at the render site.
//! See the co-located `DESIGN.md`.

use super::error::ContextError;
use crate::orchestration::bounding::NamedCheckWidth;

/// The identity of the verification a task's success depends on: what is
/// checked and the criterion it must meet, for example
/// `per-directory entry count (max 30, recursive)`.
///
/// Bounded and non-empty by construction. The bound keeps the identity to a
/// decisive one-liner; bulk description belongs in the task text, not here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CheckIdentity(String);

impl CheckIdentity {
    /// Parse a check identity.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyCheckIdentity`] when the text is empty or
    /// whitespace-only, and [`ContextError::CheckIdentityTooLong`] when it
    /// exceeds [`NamedCheckWidth::DEFAULT`].
    pub fn new(raw: &str) -> Result<Self, ContextError> {
        if raw.trim().is_empty() {
            return Err(ContextError::EmptyCheckIdentity);
        }
        if !NamedCheckWidth::DEFAULT.fits(raw) {
            return Err(ContextError::CheckIdentityTooLong);
        }
        Ok(Self(raw.to_owned()))
    }

    /// The check identity text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CheckIdentity {
    type Error = ContextError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

/// The decisive result a named check produced: the datum that settles pass or
/// fail — a count, a delta, an exit line — never the bulk transcript.
///
/// Bounded and non-empty by construction. Anything over the bound is rejected
/// at construction so the field cannot become the dumping ground the size
/// bound exists to prevent (packet section 7); bulk output references the
/// spilled artifact instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CheckResult(String);

impl CheckResult {
    /// Parse a decisive check result.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyCheckResult`] when the text is empty or
    /// whitespace-only, and [`ContextError::CheckResultTooLong`] when it
    /// exceeds [`NamedCheckWidth::DEFAULT`].
    pub fn new(raw: &str) -> Result<Self, ContextError> {
        if raw.trim().is_empty() {
            return Err(ContextError::EmptyCheckResult);
        }
        if !NamedCheckWidth::DEFAULT.fits(raw) {
            return Err(ContextError::CheckResultTooLong);
        }
        Ok(Self(raw.to_owned()))
    }

    /// The decisive result text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CheckResult {
    type Error = ContextError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

/// The outcome of a named check: either the decisive result it produced, or an
/// explicit record that it was not run.
///
/// `NOT RUN` is a variant, never an empty [`CheckResult`]: a check that a task
/// named but the worker did not perform is a distinct, representable state
/// (packet section 8), so a blank result can never masquerade as a run check.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CheckOutcome {
    /// The check was performed; this is the decisive result it produced.
    Performed(CheckResult),
    /// The task named the check but the worker did not perform it: either the
    /// worker reported it could not, or a declared check was absent from the
    /// result entirely and was reconciled to this state at the render site.
    NotRun,
}

/// A named check paired with its outcome: the two-ended verification evidence
/// this card enforces.
///
/// The identity is always present, so a decisive result can never travel
/// divorced from the check it belongs to. When the task named a check but the
/// worker carried nothing for it, the outcome is [`CheckOutcome::NotRun`] —
/// the check cannot be silently absent, it renders `NOT RUN`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NamedCheck {
    identity: CheckIdentity,
    outcome: CheckOutcome,
}

impl NamedCheck {
    /// Parse a named check from a worker's raw submission: the check identity,
    /// and the decisive result when the worker performed it (`None` records
    /// that the worker named the check but did not run it).
    ///
    /// # Errors
    ///
    /// Propagates [`CheckIdentity::new`] and [`CheckResult::new`] errors.
    pub fn parse(check: &str, result: Option<&str>) -> Result<Self, ContextError> {
        let identity = CheckIdentity::new(check)?;
        let outcome = match result {
            Some(result) => CheckOutcome::Performed(CheckResult::new(result)?),
            None => CheckOutcome::NotRun,
        };
        Ok(Self { identity, outcome })
    }

    /// Build a named check for a task that declared a check the worker's
    /// result did not carry: the outcome is [`CheckOutcome::NotRun`].
    ///
    /// # Errors
    ///
    /// Propagates [`CheckIdentity::new`] errors.
    pub fn not_run(check: &str) -> Result<Self, ContextError> {
        Ok(Self {
            identity: CheckIdentity::new(check)?,
            outcome: CheckOutcome::NotRun,
        })
    }

    /// The check identity.
    pub fn identity(&self) -> &CheckIdentity {
        &self.identity
    }

    /// The check outcome.
    pub fn outcome(&self) -> &CheckOutcome {
        &self.outcome
    }

    /// Render the decisive check line for an evidence entry:
    /// `[Check: {identity} -> {result}]`, or `[Check: {identity} -> NOT RUN]`
    /// when the check was not run (packet section 8).
    #[must_use]
    pub fn render_line(&self) -> String {
        todo!("S46 phase 2: render the [Check: ...] line; see named_check/DESIGN.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_identity_and_result_are_rejected() {
        assert_eq!(CheckIdentity::new("  "), Err(ContextError::EmptyCheckIdentity));
        assert_eq!(CheckResult::new(""), Err(ContextError::EmptyCheckResult));
    }

    #[test]
    fn over_bound_fields_are_rejected() {
        let long = "x".repeat(10_000);
        assert!(!NamedCheckWidth::DEFAULT.fits(&long));
        assert_eq!(
            CheckIdentity::new(&long),
            Err(ContextError::CheckIdentityTooLong)
        );
        assert_eq!(CheckResult::new(&long), Err(ContextError::CheckResultTooLong));
    }

    #[test]
    fn parse_maps_absent_result_to_not_run() {
        let performed = NamedCheck::parse("per-directory entry count (max 30)", Some("VIOLATION: g00000 has 53"))
            .expect("valid check");
        assert!(matches!(performed.outcome(), CheckOutcome::Performed(_)));

        let absent = NamedCheck::parse("per-directory entry count (max 30)", None).expect("valid check");
        assert_eq!(absent.outcome(), &CheckOutcome::NotRun);
    }
}
