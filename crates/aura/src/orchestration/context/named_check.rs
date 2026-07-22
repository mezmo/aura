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
//! `NOT RUN` — happens at the render site via [`NamedCheck::reconcile`].
//!
//! # Design status
//!
//! S46 phase 2 landed the bodies over the phase-1 skeleton. The parsing
//! constructors ([`CheckIdentity::new`], [`CheckResult::new`],
//! [`NamedCheck::parse`]), the render leg ([`NamedCheck::render_line`]), the
//! render-site reconciliation ([`NamedCheck::reconcile`]), and the coordinator's
//! acceptance predicate ([`declared_check_satisfied`]) are all implemented. See
//! the co-located `DESIGN.md`.

use super::error::ContextError;
use crate::orchestration::bounding::NamedCheckWidth;
use crate::orchestration::tools::submit_result::SubmittedCheck;

/// Stand-in identity rendered when a worker's submission carried a check whose
/// identity was itself unrepresentable and no task declaration is available to
/// render against: the malformed submission is surfaced, never silently absent
/// (design-panel RV1).
const UNREPRESENTABLE_SUBMISSION_IDENTITY: &str = "submitted check (unrepresentable identity)";

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
    /// `[Check: {identity} -> {outcome}]` (packet section 8). The outcome is the
    /// decisive result when the check was performed, the worker's observation
    /// prefixed `COULD NOT PERFORM:` when it could not, and `NOT RUN` when a
    /// declared check carried nothing.
    #[must_use]
    pub fn render_line(&self) -> String {
        let outcome = match &self.outcome {
            CheckOutcome::Performed(result) => result.as_str().to_owned(),
            CheckOutcome::Incapable(observed) => format!("COULD NOT PERFORM: {observed}"),
            CheckOutcome::NotRun => "NOT RUN".to_owned(),
        };
        format!("[Check: {} -> {}]", self.identity, outcome)
    }

    /// Reconcile a task's declared check identity against what the worker's
    /// submission carried, yielding the check to render for one evidence entry
    /// (design-panel P4). Reconciliation happens at the render site, so a
    /// declared check the worker did not carry renders `NOT RUN` on every entry
    /// shape — inline, spilled, or claimless.
    ///
    /// It is declaration-driven (packet section 8): a task that declared a check
    /// always renders an outcome — the worker's carried result when the
    /// identities match, `NOT RUN` otherwise (mismatched identity, absent, or an
    /// unrepresentable submission) — so a declared check the worker did not
    /// answer can never render as a clean success. A task that declared no check
    /// renders no line, with one exception: a submission whose identity was
    /// itself unrepresentable is surfaced rather than dropped (design-panel
    /// RV1), so a malformed submission never vanishes silently.
    #[must_use]
    pub fn reconcile(
        declaration: Option<&CheckIdentity>,
        submitted: Option<&SubmittedCheck>,
    ) -> Option<Self> {
        match declaration {
            Some(identity) => Some(match submitted {
                Some(SubmittedCheck::Present(carried)) if carried.identity() == identity => {
                    carried.clone()
                }
                _ => Self {
                    identity: identity.clone(),
                    outcome: CheckOutcome::NotRun,
                },
            }),
            None => match submitted {
                Some(SubmittedCheck::UnrepresentableIdentity) => {
                    Self::not_run(UNREPRESENTABLE_SUBMISSION_IDENTITY).ok()
                }
                _ => None,
            },
        }
    }
}

/// Whether a task's declared check is satisfied by the worker's submission
/// (design-panel P2): identity equality with the declaration AND a performed
/// outcome. Incapable, `NotRun`, a mismatched identity, an unrepresentable
/// submission, and an absent one are all unverified. A task that declared no
/// check is trivially satisfied.
///
/// This is the coordinator's acceptance obligation: a declared-check task is
/// accepted only on the named check's own performed result, never on the
/// worker's self-report (packet section 2, Rule 2). Anything else surfaces as
/// unverified in the coordinator's view (the reconciled `NOT RUN` /
/// `COULD NOT PERFORM` render line).
#[must_use]
pub fn declared_check_satisfied(
    declaration: Option<&CheckIdentity>,
    submitted: Option<&SubmittedCheck>,
) -> bool {
    match declaration {
        None => true,
        Some(identity) => matches!(
            submitted,
            Some(SubmittedCheck::Present(carried))
                if carried.identity() == identity
                    && matches!(carried.outcome(), CheckOutcome::Performed(_))
        ),
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

    // Phase-2 render: the `[Check: {identity} -> {outcome}]` line renders each
    // outcome distinctly — the decisive result, a `COULD NOT PERFORM:` prefix,
    // and the explicit `NOT RUN` (packet section 8).
    #[test]
    fn render_line_distinguishes_every_outcome() {
        let performed = NamedCheck::parse(
            "per-directory entry count (max 30)",
            Some("VIOLATION: g00000 has 53"),
            None,
        )
        .expect("valid");
        assert_eq!(
            performed.render_line(),
            "[Check: per-directory entry count (max 30) -> VIOLATION: g00000 has 53]"
        );

        let incapable = NamedCheck::parse(
            "desktop framebuffer read",
            None,
            Some("sandbox denied the monitor socket"),
        )
        .expect("valid");
        assert_eq!(
            incapable.render_line(),
            "[Check: desktop framebuffer read -> COULD NOT PERFORM: sandbox denied the monitor socket]"
        );

        let not_run = NamedCheck::not_run("per-directory entry count (max 30)").expect("valid");
        assert_eq!(
            not_run.render_line(),
            "[Check: per-directory entry count (max 30) -> NOT RUN]"
        );
    }

    // P4: a task that declared no check reconciles to no line, so checkless
    // tasks stay clean (the negative-space clause).
    #[test]
    fn reconcile_no_declaration_yields_no_line() {
        assert_eq!(NamedCheck::reconcile(None, None), None);
        assert_eq!(
            NamedCheck::reconcile(None, Some(&SubmittedCheck::Absent)),
            None
        );
        let present =
            SubmittedCheck::Present(NamedCheck::parse("count", Some("30"), None).expect("valid"));
        assert_eq!(NamedCheck::reconcile(None, Some(&present)), None);
    }

    // P4: a declared check whose identity matches the worker's carried check
    // renders the worker's outcome; the deciding result is preserved.
    #[test]
    fn reconcile_matching_identity_carries_worker_outcome() {
        let declaration = CheckIdentity::new("per-directory entry count (max 30)").expect("valid");
        let submitted = SubmittedCheck::Present(
            NamedCheck::parse(
                "per-directory entry count (max 30)",
                Some("VIOLATION: g00000 has 53"),
                None,
            )
            .expect("valid"),
        );
        let reconciled =
            NamedCheck::reconcile(Some(&declaration), Some(&submitted)).expect("declared");
        assert_eq!(
            reconciled.render_line(),
            "[Check: per-directory entry count (max 30) -> VIOLATION: g00000 has 53]"
        );
    }

    // P4/P2: a declared check the worker did not carry — absent, a mismatched
    // identity, or an unrepresentable submission — reconciles to NOT RUN against
    // the declared identity, so it can never render as a clean success.
    #[test]
    fn reconcile_uncarried_declaration_renders_not_run() {
        let declaration = CheckIdentity::new("per-directory entry count (max 30)").expect("valid");

        let absent = NamedCheck::reconcile(Some(&declaration), Some(&SubmittedCheck::Absent))
            .expect("declared");
        assert_eq!(absent.outcome(), &CheckOutcome::NotRun);

        let none = NamedCheck::reconcile(Some(&declaration), None).expect("declared");
        assert_eq!(none.outcome(), &CheckOutcome::NotRun);

        let mismatch = SubmittedCheck::Present(
            NamedCheck::parse("a different check", Some("passed"), None).expect("valid"),
        );
        let reconciled =
            NamedCheck::reconcile(Some(&declaration), Some(&mismatch)).expect("declared");
        assert_eq!(reconciled.outcome(), &CheckOutcome::NotRun);
        assert_eq!(
            reconciled.identity().as_str(),
            "per-directory entry count (max 30)"
        );

        let unrepresentable = NamedCheck::reconcile(
            Some(&declaration),
            Some(&SubmittedCheck::UnrepresentableIdentity),
        )
        .expect("declared");
        assert_eq!(unrepresentable.outcome(), &CheckOutcome::NotRun);
    }

    // RV1: an unrepresentable submission on a task that declared no check is
    // still surfaced, never silently absent.
    #[test]
    fn reconcile_surfaces_unrepresentable_submission_without_declaration() {
        let reconciled =
            NamedCheck::reconcile(None, Some(&SubmittedCheck::UnrepresentableIdentity))
                .expect("surfaced, not dropped");
        assert_eq!(reconciled.outcome(), &CheckOutcome::NotRun);
        assert!(
            reconciled.render_line().contains("NOT RUN"),
            "unrepresentable submission renders visibly: {}",
            reconciled.render_line()
        );
    }

    // P2: acceptance requires identity equality AND a performed outcome. Every
    // other shape — incapable, not-run, mismatched, unrepresentable, absent — is
    // unverified. A checkless task is trivially satisfied.
    #[test]
    fn declared_check_satisfied_requires_identity_equality_and_performed() {
        let declaration = CheckIdentity::new("per-directory entry count (max 30)").expect("valid");

        let performed = SubmittedCheck::Present(
            NamedCheck::parse("per-directory entry count (max 30)", Some("clean"), None)
                .expect("valid"),
        );
        assert!(declared_check_satisfied(
            Some(&declaration),
            Some(&performed)
        ));

        let incapable = SubmittedCheck::Present(
            NamedCheck::parse("per-directory entry count (max 30)", None, Some("denied"))
                .expect("valid"),
        );
        assert!(!declared_check_satisfied(
            Some(&declaration),
            Some(&incapable)
        ));

        let mismatch = SubmittedCheck::Present(
            NamedCheck::parse("a different check", Some("clean"), None).expect("valid"),
        );
        assert!(!declared_check_satisfied(
            Some(&declaration),
            Some(&mismatch)
        ));

        assert!(!declared_check_satisfied(
            Some(&declaration),
            Some(&SubmittedCheck::Absent)
        ));
        assert!(!declared_check_satisfied(
            Some(&declaration),
            Some(&SubmittedCheck::UnrepresentableIdentity)
        ));
        assert!(!declared_check_satisfied(Some(&declaration), None));

        // No declared check: nothing to verify, trivially satisfied.
        assert!(declared_check_satisfied(
            None,
            Some(&SubmittedCheck::Absent)
        ));
        assert!(declared_check_satisfied(None, None));
    }
}
