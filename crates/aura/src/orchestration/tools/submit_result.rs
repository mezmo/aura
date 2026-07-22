//! Structured worker output via the `submit_result` tool.
//!
//! Workers call `submit_result` as their final action to provide structured
//! output with a summary, full result, and confidence level. Uses the same
//! first-write-wins pattern as coordinator routing tools.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::orchestration::context::NamedCheck;

/// Worker-reported confidence level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Confidence::High => write!(f, "high"),
            Confidence::Medium => write!(f, "medium"),
            Confidence::Low => write!(f, "low"),
        }
    }
}

/// Structured output from a worker's `submit_result` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitResultOutput {
    pub summary: String,
    pub result: String,
    pub confidence: Confidence,
    /// What the worker's submission carried for the decisive named check: the
    /// acceptance gate's data source. The coordinator does not treat the task
    /// as done on the summary alone when a check was named. See
    /// [`SubmittedCheck`] for the three non-aliasing states.
    #[serde(default)]
    pub named_check: SubmittedCheck,
}

/// What a worker's `submit_result` call carried for the decisive named check.
///
/// Three mutually exclusive states, none aliasing another. In particular, a
/// submission whose check identity was itself unrepresentable stays distinct
/// from "no check named": a rejected submission never masquerades as an absent
/// one (design-panel RV1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum SubmittedCheck {
    /// The task named no decisive check, or the worker carried nothing for it.
    /// The acceptance gate has no check to read.
    #[default]
    Absent,
    /// A bounded named check. Its outcome is `NotRun` when the worker's carried
    /// result was rejected at the field bound but the identity itself held
    /// (design-panel P3): present-but-rejected evidence stays visible.
    Present(NamedCheck),
    /// A check was submitted, but its identity itself was empty or over the
    /// field bound, so no bounded [`NamedCheck`] could be built. Distinct from
    /// [`Absent`](Self::Absent): a check *was* named — it just could not be
    /// represented, and that unrepresentability stays visible instead of
    /// collapsing to "no check named" (design-panel RV1). The harder phase-2
    /// target — hard-reject the submission so the worker retries with a
    /// bounded identity — is recorded in the ledger, not built here.
    UnrepresentableIdentity,
}

impl SubmittedCheck {
    /// Whether the submission named no representable check. Lets the stored
    /// output skip serializing the field on the common (checkless) path, so
    /// legacy JSON stays byte-identical.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

/// Shared state for capturing a worker's structured result.
/// First-write-wins: subsequent calls are rejected.
pub type SubmitResultDecision = Arc<Mutex<Option<SubmitResultOutput>>>;

/// Tool that workers call to submit structured output.
#[derive(Clone)]
pub struct SubmitResultTool {
    decision: SubmitResultDecision,
}

impl SubmitResultTool {
    pub fn new(decision: SubmitResultDecision) -> Self {
        Self { decision }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SubmitResultArgs {
    /// Concise summary of findings (1-3 sentences). Shown to the coordinator
    /// and stored in session history.
    pub summary: String,
    /// Complete findings and analysis.
    pub result: String,
    /// Confidence in the result: "high", "medium", or "low".
    pub confidence: String,
    /// The specific verification whose result determines pass or fail, when
    /// the task named one. Omitted when the task named no such check.
    #[serde(default)]
    pub named_check: Option<NamedCheckArgs>,
}

/// Raw wire form of a worker's decisive named check: the check performed and
/// the result it produced. Parsed into the bounded [`NamedCheck`] at the tool
/// boundary.
#[derive(Debug, Deserialize, Serialize)]
pub struct NamedCheckArgs {
    /// The check that decides success: a specific verification whose result
    /// determines pass or fail.
    pub check: String,
    /// The decisive result the check actually produced. Absent when the worker
    /// could not perform it.
    #[serde(default)]
    pub result: Option<String>,
    /// What the worker observed when it could not perform the check (packet
    /// section 5c). Present only on the incapacity path, when `result` is
    /// absent; it keeps the incapacity note in a bounded structured slot rather
    /// than the free-form self-report channel. The worker-facing instruction to
    /// fill it is phase-2 (this field is unadvertised in the tool schema, like
    /// `named_check` itself).
    #[serde(default)]
    pub observed: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubmitResultToolOutput {
    pub status: String,
}

impl std::fmt::Display for SubmitResultToolOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.status)
    }
}

impl Tool for SubmitResultTool {
    const NAME: &'static str = "submit_result";

    type Error = std::convert::Infallible;
    type Args = SubmitResultArgs;
    type Output = SubmitResultToolOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Submit your structured result. Call this once when you have \
                your final answer. Provide a concise summary for the coordinator, \
                your complete findings, and your confidence level."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "Concise summary of findings (1-3 sentences). This becomes the preview shown to the coordinator and stored in session history."
                    },
                    "result": {
                        "type": "string",
                        "description": "Complete findings and analysis."
                    },
                    "confidence": {
                        "type": "string",
                        "enum": ["high", "medium", "low"],
                        "description": "Confidence in the result. 'low' if key data was unavailable or ambiguous."
                    },
                    "named_check": {
                        "type": "object",
                        "description": "The specific verification whose result decides this task's success, when the task named one. Omit entirely when the task named no such check. Fill it by performing the check, not by reasoning about what it would produce.",
                        "properties": {
                            "check": {
                                "type": "string",
                                "description": "The check that decides success: a specific verification whose result determines pass or fail, e.g. 'per-directory entry count (max 30, recursive)'. Keep it to one decisive line."
                            },
                            "result": {
                                "type": "string",
                                "description": "The decisive result the check actually produced — a count, a delta, a pass/fail line. Provide this when you performed the check; keep bulk output in `result` and reference it here by its decisive datum only."
                            },
                            "observed": {
                                "type": "string",
                                "description": "What you observed when you could NOT perform the check. Provide this instead of `result` on the incapacity path, and never claim the check passed."
                            }
                        },
                        "required": ["check"]
                    }
                },
                "required": ["summary", "result", "confidence"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut guard = self.decision.lock().await;
        if guard.is_some() {
            tracing::warn!(
                summary = %args.summary,
                "submit_result called again (duplicate, first submission kept)"
            );
            return Ok(SubmitResultToolOutput {
                status: "Result already submitted (first submission kept).".to_string(),
            });
        }

        tracing::info!(
            summary = %args.summary,
            confidence = %args.confidence,
            result_len = args.result.len(),
            "submit_result called (first submission accepted)"
        );

        let confidence = match args.confidence.to_lowercase().as_str() {
            "high" => Confidence::High,
            "medium" => Confidence::Medium,
            "low" => Confidence::Low,
            _ => Confidence::Medium, // default for unexpected values
        };

        // Present-but-rejected evidence must never alias "no check named": when
        // the worker's carried result fails the field bound, preserve the
        // declared identity as NotRun rather than dropping the whole check
        // (design-panel P3). When the identity itself is unrepresentable, the
        // submission still leaves a typed trace (UnrepresentableIdentity, RV1),
        // never collapsing to Absent. A hard-reject-and-retry path is phase-2.
        let named_check = match &args.named_check {
            None => SubmittedCheck::Absent,
            Some(nc) => {
                // RV2: a worker that carried both a decisive result and an
                // incapacity observation supplies an ambiguous dual payload.
                // The decisive result takes precedence (a performed check
                // settles pass/fail); the drop of the observation is diagnosed
                // here rather than vanishing silently inside `parse`. The
                // error-channel alternative would need a new `ContextError`
                // variant, outside this repair's file scope.
                if nc.result.is_some() && nc.observed.is_some() {
                    tracing::warn!(
                        "submit_result named_check carried both a decisive result and an \
                         incapacity observation; the decisive result takes precedence and \
                         the observation is not retained"
                    );
                }
                match NamedCheck::parse(&nc.check, nc.result.as_deref(), nc.observed.as_deref()) {
                    Ok(parsed) => SubmittedCheck::Present(parsed),
                    Err(error) => match NamedCheck::not_run(&nc.check) {
                        Ok(preserved) => {
                            tracing::warn!(
                                %error,
                                "submit_result named_check evidence rejected at the field bound; \
                                 preserving the declared identity as NOT RUN"
                            );
                            SubmittedCheck::Present(preserved)
                        }
                        Err(identity_error) => {
                            // RV1: identity itself empty or over-bound, so no
                            // bounded NamedCheck exists. Record the submission
                            // as unrepresentable rather than dropping to Absent.
                            tracing::warn!(
                                %error,
                                %identity_error,
                                "submit_result named_check identity itself is unrepresentable; \
                                 recording an unrepresentable-identity trace, not aliasing absent"
                            );
                            SubmittedCheck::UnrepresentableIdentity
                        }
                    },
                }
            }
        };

        *guard = Some(SubmitResultOutput {
            summary: args.summary,
            result: args.result,
            confidence,
            named_check,
        });

        Ok(SubmitResultToolOutput {
            status: "Result submitted.".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::tool::Tool;

    #[tokio::test]
    async fn test_first_write_wins() {
        let decision: SubmitResultDecision = Arc::new(Mutex::new(None));
        let tool = SubmitResultTool::new(decision.clone());

        let result = tool
            .call(SubmitResultArgs {
                summary: "Found 42 errors".to_string(),
                result: "Full analysis...".to_string(),
                confidence: "high".to_string(),
                named_check: None,
            })
            .await
            .unwrap();
        assert_eq!(result.status, "Result submitted.");

        let stored = decision.lock().await;
        assert!(stored.is_some());
        let output = stored.as_ref().unwrap();
        assert_eq!(output.summary, "Found 42 errors");
        assert_eq!(output.result, "Full analysis...");
        assert_eq!(output.confidence, Confidence::High);
    }

    #[tokio::test]
    async fn test_second_call_rejected() {
        let decision: SubmitResultDecision = Arc::new(Mutex::new(None));
        let tool = SubmitResultTool::new(decision.clone());

        tool.call(SubmitResultArgs {
            summary: "First".to_string(),
            result: "First result".to_string(),
            confidence: "high".to_string(),
            named_check: None,
        })
        .await
        .unwrap();

        let result = tool
            .call(SubmitResultArgs {
                summary: "Second".to_string(),
                result: "Second result".to_string(),
                confidence: "low".to_string(),
                named_check: None,
            })
            .await
            .unwrap();
        assert!(result.status.contains("already submitted"));

        let stored = decision.lock().await;
        let output = stored.as_ref().unwrap();
        assert_eq!(output.summary, "First");
    }

    // P10: a pre-S46 worker payload carries no `named_check` field. It must
    // still deserialize, defaulting the check to absent.
    #[test]
    fn submit_result_args_deserializes_legacy_payload_without_named_check() {
        let json = r#"{"summary":"done","result":"full findings","confidence":"high"}"#;
        let args: SubmitResultArgs = serde_json::from_str(json).unwrap();
        assert!(args.named_check.is_none());
    }

    // P10: a named_check without the newer `observed` field (result-only, the
    // shape a first-generation OPTION-IN worker sends) still deserializes.
    #[test]
    fn named_check_args_deserializes_without_observed_field() {
        let json = r#"{"check":"entry count","result":"30 entries"}"#;
        let args: NamedCheckArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.check, "entry count");
        assert_eq!(args.result.as_deref(), Some("30 entries"));
        assert!(args.observed.is_none());
    }

    // P10: the stored output round-trips, and a legacy stored output without
    // `named_check` deserializes to absent.
    #[test]
    fn submit_result_output_deserializes_legacy_payload() {
        let json = r#"{"summary":"s","result":"r","confidence":"medium"}"#;
        let output: SubmitResultOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(output.named_check, SubmittedCheck::Absent));
    }

    // P3: an over-bound decisive result is rejected at the field bound, but the
    // declared identity is preserved as NOT RUN — present-but-rejected must
    // never alias "no check named".
    #[tokio::test]
    async fn over_bound_result_is_preserved_as_not_run_not_dropped() {
        use crate::orchestration::context::CheckOutcome;

        let decision: SubmitResultDecision = Arc::new(Mutex::new(None));
        let tool = SubmitResultTool::new(decision.clone());

        tool.call(SubmitResultArgs {
            summary: "checked".to_string(),
            result: "full findings".to_string(),
            confidence: "high".to_string(),
            named_check: Some(NamedCheckArgs {
                check: "per-directory entry count (max 30)".to_string(),
                result: Some("x".repeat(10_000)),
                observed: None,
            }),
        })
        .await
        .unwrap();

        let stored = decision.lock().await;
        let named_check = match &stored.as_ref().unwrap().named_check {
            SubmittedCheck::Present(nc) => nc,
            other => panic!("identity preserved as Present, not dropped, got {other:?}"),
        };
        assert_eq!(
            named_check.identity().as_str(),
            "per-directory entry count (max 30)"
        );
        assert_eq!(named_check.outcome(), &CheckOutcome::NotRun);
    }

    // P5: the observed-only incapacity path through the tool's own `call`. A
    // submission whose named_check carries a check and an `observed` datum but
    // no `result` must store `Present` with `CheckOutcome::Incapable` carrying
    // the observation — never `NotRun` (which would alias reconciled-absent).
    #[tokio::test]
    async fn observed_only_stores_incapable_carrying_the_observation() {
        use crate::orchestration::context::CheckOutcome;

        let decision: SubmitResultDecision = Arc::new(Mutex::new(None));
        let tool = SubmitResultTool::new(decision.clone());

        tool.call(SubmitResultArgs {
            summary: "could not run the check".to_string(),
            result: "full findings".to_string(),
            confidence: "low".to_string(),
            named_check: Some(NamedCheckArgs {
                check: "per-directory entry count (max 30)".to_string(),
                result: None,
                observed: Some("sandbox denied directory listing".to_string()),
            }),
        })
        .await
        .unwrap();

        let stored = decision.lock().await;
        let named_check = match &stored.as_ref().unwrap().named_check {
            SubmittedCheck::Present(nc) => nc,
            other => panic!("observed-only stores Present, got {other:?}"),
        };
        assert_eq!(
            named_check.identity().as_str(),
            "per-directory entry count (max 30)"
        );
        match named_check.outcome() {
            CheckOutcome::Incapable(observed) => {
                assert_eq!(observed.as_str(), "sandbox denied directory listing");
            }
            other => panic!("observed-only outcome must be Incapable, got {other:?}"),
        }
    }

    // RV1: when the submitted check identity itself is over-bound, no bounded
    // NamedCheck exists to preserve — but the submission must not alias "no
    // check named". It records UnrepresentableIdentity, distinct from Absent.
    #[tokio::test]
    async fn over_bound_identity_records_unrepresentable_not_absent() {
        let decision: SubmitResultDecision = Arc::new(Mutex::new(None));
        let tool = SubmitResultTool::new(decision.clone());

        tool.call(SubmitResultArgs {
            summary: "checked".to_string(),
            result: "full findings".to_string(),
            confidence: "high".to_string(),
            named_check: Some(NamedCheckArgs {
                check: "x".repeat(10_000),
                result: Some("30 entries".to_string()),
                observed: None,
            }),
        })
        .await
        .unwrap();

        let stored = decision.lock().await;
        let submitted = &stored.as_ref().unwrap().named_check;
        assert!(
            matches!(submitted, SubmittedCheck::UnrepresentableIdentity),
            "over-bound identity must record an unrepresentable trace, not alias absent, \
             got {submitted:?}"
        );
    }

    // RV1: the empty-identity submission path is the other unrepresentable
    // identity — it too records a trace rather than aliasing absent.
    #[tokio::test]
    async fn empty_identity_records_unrepresentable_not_absent() {
        let decision: SubmitResultDecision = Arc::new(Mutex::new(None));
        let tool = SubmitResultTool::new(decision.clone());

        tool.call(SubmitResultArgs {
            summary: "checked".to_string(),
            result: "full findings".to_string(),
            confidence: "high".to_string(),
            named_check: Some(NamedCheckArgs {
                check: "   ".to_string(),
                result: Some("30 entries".to_string()),
                observed: None,
            }),
        })
        .await
        .unwrap();

        let stored = decision.lock().await;
        let submitted = &stored.as_ref().unwrap().named_check;
        assert!(
            matches!(submitted, SubmittedCheck::UnrepresentableIdentity),
            "empty identity must record an unrepresentable trace, not alias absent, \
             got {submitted:?}"
        );
    }
}
