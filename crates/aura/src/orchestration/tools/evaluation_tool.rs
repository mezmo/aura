//! Evaluation tool for structured quality assessment.
//!
//! The evaluation coordinator calls `submit_evaluation` to record its
//! quality assessment of a synthesized response. A shared `EvaluationDecision`
//! captures the result for the orchestrator to read (first-write-wins).

use crate::orchestration::types::EvaluationResult;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared state for the evaluation coordinator's decision.
///
/// The `submit_evaluation` tool stores its result here on first call.
/// Subsequent calls are rejected (first-write-wins).
pub type EvaluationDecision = Arc<Mutex<Option<EvaluationResult>>>;

/// Tool for submitting a structured evaluation result.
#[derive(Clone)]
pub struct SubmitEvaluationTool {
    decision: EvaluationDecision,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SubmitEvaluationArgs {
    /// Quality score between 0.0 and 1.0.
    pub score: f32,
    /// Brief explanation of the evaluation.
    pub reasoning: String,
    /// Identified gaps or missing elements in the response.
    #[serde(default)]
    pub gaps: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SubmitEvaluationOutput {
    pub status: String,
}

impl std::fmt::Display for SubmitEvaluationOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.status)
    }
}

impl Default for SubmitEvaluationTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SubmitEvaluationTool {
    /// Create a new evaluation tool with fresh shared state.
    pub fn new() -> Self {
        Self {
            decision: Arc::new(Mutex::new(None)),
        }
    }

    /// Get a clone of the shared decision arc for reading.
    pub fn decision(&self) -> EvaluationDecision {
        self.decision.clone()
    }
}

impl Tool for SubmitEvaluationTool {
    const NAME: &'static str = "submit_evaluation";

    type Error = Infallible;
    type Args = SubmitEvaluationArgs;
    type Output = SubmitEvaluationOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Submit your evaluation of the synthesized response. \
                Call this exactly once with your quality assessment."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "score": {
                        "type": "number",
                        "description": "Quality score between 0.0 (completely wrong) and 1.0 (perfect)",
                        "minimum": 0.0,
                        "maximum": 1.0
                    },
                    "reasoning": {
                        "type": "string",
                        "description": "Brief explanation of your score"
                    },
                    "gaps": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Missing elements in the response (empty array if none)"
                    }
                },
                "required": ["score", "reasoning"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut guard = self.decision.lock().await;
        if guard.is_some() {
            return Ok(SubmitEvaluationOutput {
                status: "Error: Evaluation already submitted. Only one submission is allowed."
                    .to_string(),
            });
        }
        *guard = Some(EvaluationResult::new(args.score, args.reasoning).with_gaps(args.gaps));
        Ok(SubmitEvaluationOutput {
            status: "Evaluation recorded.".to_string(),
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stores_evaluation_result() {
        let tool = SubmitEvaluationTool::new();
        let decision = tool.decision();

        let result = tool
            .call(SubmitEvaluationArgs {
                score: 0.85,
                reasoning: "Good response".to_string(),
                gaps: vec!["Minor detail missing".to_string()],
            })
            .await
            .unwrap();

        assert!(result.status.contains("recorded"));

        let guard = decision.lock().await;
        let eval = guard.as_ref().unwrap();
        assert!((eval.score - 0.85).abs() < f32::EPSILON);
        assert_eq!(eval.reasoning, "Good response");
        assert_eq!(eval.gaps, vec!["Minor detail missing"]);
    }

    #[tokio::test]
    async fn test_first_write_wins() {
        let tool = SubmitEvaluationTool::new();
        let decision = tool.decision();

        // First call succeeds
        let result = tool
            .call(SubmitEvaluationArgs {
                score: 0.9,
                reasoning: "First".to_string(),
                gaps: vec![],
            })
            .await
            .unwrap();
        assert!(result.status.contains("recorded"));

        // Second call is rejected
        let result = tool
            .call(SubmitEvaluationArgs {
                score: 0.5,
                reasoning: "Second".to_string(),
                gaps: vec![],
            })
            .await
            .unwrap();
        assert!(result.status.contains("already submitted"));

        // Original decision preserved
        let guard = decision.lock().await;
        let eval = guard.as_ref().unwrap();
        assert!((eval.score - 0.9).abs() < f32::EPSILON);
        assert_eq!(eval.reasoning, "First");
    }

    #[tokio::test]
    async fn test_score_clamping() {
        // Score > 1.0 gets clamped
        let tool = SubmitEvaluationTool::new();
        let decision = tool.decision();
        tool.call(SubmitEvaluationArgs {
            score: 1.5,
            reasoning: "Too high".to_string(),
            gaps: vec![],
        })
        .await
        .unwrap();

        let guard = decision.lock().await;
        let eval = guard.as_ref().unwrap();
        assert!((eval.score - 1.0).abs() < f32::EPSILON);
        drop(guard);

        // Score < 0.0 gets clamped
        let tool = SubmitEvaluationTool::new();
        let decision = tool.decision();
        tool.call(SubmitEvaluationArgs {
            score: -0.5,
            reasoning: "Too low".to_string(),
            gaps: vec![],
        })
        .await
        .unwrap();

        let guard = decision.lock().await;
        let eval = guard.as_ref().unwrap();
        assert!(eval.score.abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_default_gaps() {
        let tool = SubmitEvaluationTool::new();
        let decision = tool.decision();
        tool.call(SubmitEvaluationArgs {
            score: 1.0,
            reasoning: "Perfect".to_string(),
            gaps: vec![], // default from serde
        })
        .await
        .unwrap();

        let guard = decision.lock().await;
        let eval = guard.as_ref().unwrap();
        assert!(eval.gaps.is_empty());
    }

    #[tokio::test]
    async fn test_tool_definition() {
        let tool = SubmitEvaluationTool::new();
        let def = tool.definition("".to_string()).await;
        assert_eq!(def.name, "submit_evaluation");
        assert!(def.description.contains("evaluation"));
    }
}
