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
    /// The decisive verification whose result determines pass or fail, when
    /// the task named one: the check performed and the result it produced.
    /// Absent when the task named no such check. This is the acceptance
    /// gate's data source — the coordinator does not treat the task as done on
    /// the summary alone when a check was named.
    #[serde(default)]
    pub named_check: Option<NamedCheck>,
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
        // declared identity as NotRun rather than dropping the whole check to
        // None (design-panel P3). A hard-reject-and-retry path is phase-2; here
        // the deciding datum's absence stays visible instead of vanishing.
        let named_check = match &args.named_check {
            Some(nc) => {
                match NamedCheck::parse(&nc.check, nc.result.as_deref(), nc.observed.as_deref()) {
                    Ok(parsed) => Some(parsed),
                    Err(error) => match NamedCheck::not_run(&nc.check) {
                        Ok(preserved) => {
                            tracing::warn!(
                                %error,
                                "submit_result named_check evidence rejected at the field bound; \
                                 preserving the declared identity as NOT RUN"
                            );
                            Some(preserved)
                        }
                        Err(identity_error) => {
                            tracing::warn!(
                                %error,
                                %identity_error,
                                "submit_result named_check identity itself is unbounded; dropping"
                            );
                            None
                        }
                    },
                }
            }
            None => None,
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
        assert!(output.named_check.is_none());
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
        let named_check = stored
            .as_ref()
            .unwrap()
            .named_check
            .as_ref()
            .expect("identity preserved, not dropped to None");
        assert_eq!(
            named_check.identity().as_str(),
            "per-directory entry count (max 30)"
        );
        assert_eq!(named_check.outcome(), &CheckOutcome::NotRun);
    }
}
