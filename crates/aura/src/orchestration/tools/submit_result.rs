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

    async fn call(
        &self,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let mut guard = self.decision.lock().await;
        if guard.is_some() {
            return Ok(SubmitResultToolOutput {
                status: "Result already submitted (first submission kept).".to_string(),
            });
        }

        let confidence = match args.confidence.to_lowercase().as_str() {
            "high" => Confidence::High,
            "medium" => Confidence::Medium,
            "low" => Confidence::Low,
            _ => Confidence::Medium, // default for unexpected values
        };

        *guard = Some(SubmitResultOutput {
            summary: args.summary,
            result: args.result,
            confidence,
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
        })
        .await
        .unwrap();

        let result = tool
            .call(SubmitResultArgs {
                summary: "Second".to_string(),
                result: "Second result".to_string(),
                confidence: "low".to_string(),
            })
            .await
            .unwrap();
        assert!(result.status.contains("already submitted"));

        let stored = decision.lock().await;
        let output = stored.as_ref().unwrap();
        assert_eq!(output.summary, "First");
    }
}
