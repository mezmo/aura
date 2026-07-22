//! The named check: the verification a task's success depends on, paired with
//! what the worker's result carried for it.
//!
//! The enforcement mechanism has two ends — a task MAY name a check that
//! decides its success at plan creation, and the worker's result carries that
//! check's outcome at `submit_result`
//! (`docs/redesign/2026-07-21-s46-enforcement-mockups.md` section 2, Rule 1).
//! This module models one of those ends: the worker-side evidence.
//! [`NamedCheck`] pairs the check identity with the outcome the worker carried,
//! bounded so it stays a single decisive line that survives result spill
//! (packet section 7) rather than growing into the bulk transcript. The
//! task-side declaration lives on `Task` (`named_check_declaration`), and
//! reconciling the two ends — a declared check the worker did not carry renders
//! `NOT RUN` — is the phase-2 render body.
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
    /// Check that `raw` satisfies the bound, without allocating. Shared by the
    /// borrowing [`new`](Self::new) and the owning [`TryFrom<String>`] path so
    /// the owned constructor can move its input in after validation.
    fn validate(raw: &str) -> Result<(), ContextError> {
        if raw.trim().is_empty() {
            return Err(ContextError::EmptyCheckIdentity);
        }
        if !NamedCheckWidth::DEFAULT.fits(raw) {
            return Err(ContextError::CheckIdentityTooLong);
        }
        Ok(())
    }

    /// Parse a check identity.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyCheckIdentity`] when the text is empty or
    /// whitespace-only, and [`ContextError::CheckIdentityTooLong`] when it
    /// exceeds [`NamedCheckWidth::DEFAULT`].
    pub fn new(raw: &str) -> Result<Self, ContextError> {
        Self::validate(raw)?;
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
        Self::validate(&raw)?;
        Ok(Self(raw))
    }
}

/// The decisive result a named check produced: the datum that settles pass or
/// fail — a count, a delta, a pass/fail line — never the bulk transcript.
///
/// Bounded and non-empty by construction. Anything over the bound is rejected
/// at construction so the field cannot become the dumping ground the size
/// bound exists to prevent (packet section 7); bulk output references the
/// spilled artifact instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CheckResult(String);

impl CheckResult {
    /// Check that `raw` satisfies the bound, without allocating. Shared by the
    /// borrowing [`new`](Self::new) and the owning [`TryFrom<String>`] path so
    /// the owned constructor can move its input in after validation.
    fn validate(raw: &str) -> Result<(), ContextError> {
        if raw.trim().is_empty() {
            return Err(ContextError::EmptyCheckResult);
        }
        if !NamedCheckWidth::DEFAULT.fits(raw) {
            return Err(ContextError::CheckResultTooLong);
        }
        Ok(())
    }

    /// Parse a decisive check result.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyCheckResult`] when the text is empty or
    /// whitespace-only, and [`ContextError::CheckResultTooLong`] when it
    /// exceeds [`NamedCheckWidth::DEFAULT`].
    pub fn new(raw: &str) -> Result<Self, ContextError> {
        Self::validate(raw)?;
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
        Self::validate(&raw)?;
        Ok(Self(raw))
    }
}

/// The outcome of a named check: the decisive result it produced, an
/// observation from a worker that could not perform it, or an explicit record
/// that it was not run.
///
/// `NOT RUN` is a variant, never an empty [`CheckResult`]: a check that a task
/// named but the worker did not perform is a distinct, representable state
/// (packet section 8), so a blank result can never masquerade as a run check.
///
/// [`Incapable`](Self::Incapable) and [`NotRun`](Self::NotRun) are kept apart
/// so the two provenances never alias. A worker that engaged the check but
/// could not complete it carries what it did observe under `Incapable`; a
/// declared check the worker's result carried nothing for — whether absent and
/// reconciled after the fact, or present but rejected at the field bound —
/// is `NotRun`, with no observation to show.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CheckOutcome {
    /// The check was performed; this is the decisive result it produced.
    Performed(CheckResult),
    /// The worker reported it could not perform the check, carrying what it did
    /// observe instead (packet section 5c). Distinct from [`NotRun`](Self::NotRun):
    /// the worker engaged the check and produced an observation.
    Incapable(CheckResult),
    /// The task named the check but the worker's result carried nothing for it:
    /// a declared check absent from the result and reconciled to this state at
    /// the render site, with no observation to show.
    NotRun,
}

/// A named check paired with its outcome: the worker-side verification evidence
/// this card enforces (one end of the two-ended mechanism; the task-side
/// declaration lives on `Task`).
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
    /// the decisive `result` when the worker performed it, and what the worker
    /// `observed` when it could not.
    ///
    /// The two optional inputs select the outcome without aliasing the
    /// provenances (packet section 5c):
    /// - a decisive `result` present → [`CheckOutcome::Performed`];
    /// - no `result` but an `observed` datum → [`CheckOutcome::Incapable`],
    ///   the worker engaged the check but could not complete it;
    /// - neither → [`CheckOutcome::NotRun`], the worker named the check but
    ///   carried nothing for it.
    ///
    /// When both `result` and `observed` are supplied — an ambiguous dual
    /// payload — the decisive `result` wins by defined precedence: a performed
    /// check settles pass or fail, so its result is the outcome. The precedence
    /// is spelled out here rather than falling out of match order, and the
    /// ambiguity is diagnosed at the wire boundary (`submit_result`) so the
    /// dropped observation never vanishes without a trace (design-panel RV2).
    ///
    /// # Errors
    ///
    /// Propagates [`CheckIdentity::new`] and [`CheckResult::new`] errors.
    pub fn parse(
        check: &str,
        result: Option<&str>,
        observed: Option<&str>,
    ) -> Result<Self, ContextError> {
        let identity = CheckIdentity::new(check)?;
        let outcome = match (result, observed) {
            // Dual-payload precedence (RV2): a decisive result wins over a
            // co-submitted observation. The `observed` slot is intentionally
            // ignored on this arm; the drop is diagnosed at the wire boundary.
            (Some(result), Some(_)) | (Some(result), None) => {
                CheckOutcome::Performed(CheckResult::new(result)?)
            }
            (None, Some(observed)) => CheckOutcome::Incapable(CheckResult::new(observed)?),
            (None, None) => CheckOutcome::NotRun,
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
        assert_eq!(
            CheckIdentity::new("  "),
            Err(ContextError::EmptyCheckIdentity)
        );
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
        assert_eq!(
            CheckResult::new(&long),
            Err(ContextError::CheckResultTooLong)
        );
    }

    #[test]
    fn parse_selects_outcome_without_aliasing_provenance() {
        let performed = NamedCheck::parse(
            "per-directory entry count (max 30)",
            Some("VIOLATION: g00000 has 53"),
            None,
        )
        .expect("valid check");
        assert!(matches!(performed.outcome(), CheckOutcome::Performed(_)));

        // Worker engaged the check but could not complete it: the observation
        // rides on Incapable, never collapsing into NotRun.
        let incapable = NamedCheck::parse(
            "per-directory entry count (max 30)",
            None,
            Some("sandbox denied recursive read under g00000"),
        )
        .expect("valid check");
        assert!(matches!(incapable.outcome(), CheckOutcome::Incapable(_)));

        // Named but nothing carried: no observation to show, so NotRun.
        let absent = NamedCheck::parse("per-directory entry count (max 30)", None, None)
            .expect("valid check");
        assert_eq!(absent.outcome(), &CheckOutcome::NotRun);
    }

    // RV2: a dual payload (both a decisive result and an observation) resolves
    // by defined precedence to the decisive result — never silently to
    // Incapable, and never dropping the result. The dropped observation is
    // diagnosed at the wire boundary, not here.
    #[test]
    fn parse_prefers_result_over_observed_on_dual_payload() {
        let both = NamedCheck::parse(
            "per-directory entry count (max 30)",
            Some("VIOLATION: g00000 has 53"),
            Some("also could not read g00001"),
        )
        .expect("valid check");
        match both.outcome() {
            CheckOutcome::Performed(result) => {
                assert_eq!(result.as_str(), "VIOLATION: g00000 has 53");
            }
            other => panic!("expected Performed with the decisive result, got {other:?}"),
        }
    }

    #[test]
    fn not_run_constructor_yields_reconciled_absent() {
        let reconciled = NamedCheck::not_run("per-directory entry count (max 30)").expect("valid");
        assert_eq!(reconciled.outcome(), &CheckOutcome::NotRun);
    }

    // P10: bounded values reject over-cap input on deserialization rather than
    // truncating it — the `#[serde(try_from = "String")]` path runs the same
    // bound as the borrowing constructor.
    #[test]
    fn deserialize_rejects_over_bound_identity_and_result() {
        let long = "x".repeat(10_000);
        let payload = format!("\"{long}\"");

        let identity: Result<CheckIdentity, _> = serde_json::from_str(&payload);
        assert!(
            identity.is_err(),
            "over-bound identity must be rejected, not truncated"
        );

        let result: Result<CheckResult, _> = serde_json::from_str(&payload);
        assert!(
            result.is_err(),
            "over-bound result must be rejected, not truncated"
        );
    }

    #[test]
    fn deserialize_accepts_within_bound_identity() {
        let identity: CheckIdentity =
            serde_json::from_str("\"per-directory entry count (max 30)\"").expect("within bound");
        assert_eq!(identity.as_str(), "per-directory entry count (max 30)");
    }

    // P10: the bounded domain types round-trip through serde, including the
    // split Incapable variant, so a persisted named check survives reload.
    #[test]
    fn named_check_serde_roundtrip_all_outcomes() {
        for check in [
            NamedCheck::parse("count", Some("30"), None).unwrap(),
            NamedCheck::parse("count", None, Some("could not read dir")).unwrap(),
            NamedCheck::parse("count", None, None).unwrap(),
        ] {
            let json = serde_json::to_string(&check).unwrap();
            let back: NamedCheck = serde_json::from_str(&json).unwrap();
            assert_eq!(check, back);
        }
    }
}
