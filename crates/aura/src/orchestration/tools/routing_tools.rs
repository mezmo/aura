//! Routing tools for coordinator planning decisions.
//!
//! These tools allow the coordinator to make a structured routing decision:
//! respond directly, create a multi-task plan, or request clarification.
//!
//! The coordinator calls exactly one of these tools during the planning phase.
//! A shared `RoutingDecision` captures the decision for the orchestrator to read.

use crate::orchestration::persistence::ExecutionPersistence;
use crate::orchestration::types::{PlanningResponse, StepInput, flatten_steps};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared state for the coordinator's routing decision.
///
/// The first routing tool to be called stores its result here.
/// Subsequent calls are rejected (first-write-wins).
pub type RoutingDecision = Arc<Mutex<Option<PlanningResponse>>>;

/// Groups the three routing tools and their shared decision state.
pub struct RoutingToolSet {
    pub respond_directly: RespondDirectlyTool,
    pub create_plan: CreatePlanTool,
    pub request_clarification: RequestClarificationTool,
    pub decision: RoutingDecision,
}

impl RoutingToolSet {
    pub fn new() -> Self {
        let decision: RoutingDecision = Arc::new(Mutex::new(None));
        Self {
            respond_directly: RespondDirectlyTool {
                decision: decision.clone(),
            },
            create_plan: CreatePlanTool {
                decision: decision.clone(),
                persistence: None,
            },
            request_clarification: RequestClarificationTool {
                decision: decision.clone(),
            },
            decision,
        }
    }
}

impl RoutingToolSet {
    /// Let `create_plan` validate task `artifacts` against this run's
    /// artifacts. Without it, any plan that attaches artifacts is rejected.
    pub fn with_persistence(mut self, persistence: Arc<Mutex<ExecutionPersistence>>) -> Self {
        self.create_plan.persistence = Some(persistence);
        self
    }
}

impl Default for RoutingToolSet {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// RespondDirectlyTool
// ============================================================================

/// Routing tool: answer the user's query directly without orchestration.
#[derive(Clone)]
pub struct RespondDirectlyTool {
    pub(crate) decision: RoutingDecision,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RespondDirectlyArgs {
    /// The complete response to send to the user.
    pub response: String,
    /// Why this query can be answered directly.
    pub routing_rationale: String,
    /// Short summary of the response for session history (1-2 sentences).
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RespondDirectlyOutput {
    pub status: String,
}

impl std::fmt::Display for RespondDirectlyOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.status)
    }
}

impl Tool for RespondDirectlyTool {
    const NAME: &'static str = "respond_directly";

    type Error = Infallible;
    type Args = RespondDirectlyArgs;
    type Output = RespondDirectlyOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Answer the user's query directly. At initial routing, use \
                for simple factual questions that do not require tool execution. \
                At end-of-iteration, use to synthesize the completed task results \
                into the final answer — inline all concrete findings, names, and \
                values because the user does not see task results directly."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "response": {
                        "type": "string",
                        "description": "The complete response to send to the user. This is the ONLY text the user sees — include all concrete data points, names, and values from task results."
                    },
                    "routing_rationale": {
                        "type": "string",
                        "description": "Brief explanation of why this query can be answered directly"
                    },
                    "summary": {
                        "type": "string",
                        "description": "1-2 sentence summary of your response for session history. Provide when the response is longer than a few sentences."
                    }
                },
                "required": ["response", "routing_rationale"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut guard = self.decision.lock().await;
        if guard.is_some() {
            return Ok(RespondDirectlyOutput {
                status: "Error: You have already selected a routing action. Only one routing tool may be called per turn.".to_string(),
            });
        }
        *guard = Some(PlanningResponse::Direct {
            response: args.response,
            routing_rationale: args.routing_rationale,
            response_summary: args.summary,
        });
        Ok(RespondDirectlyOutput {
            status: "Direct response recorded.".to_string(),
        })
    }
}

// ============================================================================
// CreatePlanTool
// ============================================================================

/// Routing tool: decompose the query into a multi-task plan for orchestration.
#[derive(Clone)]
pub struct CreatePlanTool {
    pub(crate) decision: RoutingDecision,
    pub(crate) persistence: Option<Arc<Mutex<ExecutionPersistence>>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreatePlanArgs {
    /// The overall goal this plan addresses.
    pub goal: String,
    /// Ordered steps to execute. Sequential by default; use `{"parallel": [...]}` for concurrency.
    pub steps: Vec<StepInput>,
    /// Why this query requires orchestration.
    pub routing_rationale: String,
    /// Natural-language summary of the plan.
    pub planning_summary: String,
}

#[derive(Debug, Serialize)]
pub struct CreatePlanOutput {
    pub status: String,
}

impl std::fmt::Display for CreatePlanOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.status)
    }
}

impl Tool for CreatePlanTool {
    const NAME: &'static str = "create_plan";

    type Error = Infallible;
    type Args = CreatePlanArgs;
    type Output = CreatePlanOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        // Tagged step schema — "type" discriminator helps models declare intent.
        let step_schema = serde_json::json!({
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["task", "parallel", "chain"],
                    "description": "Step kind: 'task' for a single task, 'parallel' for concurrent steps, 'chain' for a sequential sub-chain inside a parallel group"
                },
                "task": {
                    "type": "string",
                    "description": "What this task accomplishes. Fully resolve all references — workers do NOT see conversation history. Required when type=task."
                },
                "worker": {
                    "type": "string",
                    "description": "Name of the specialized worker to assign this task to. Required when type=task."
                },
                "artifacts": {
                    "type": "array",
                    "description": "Artifact filenames from this run to hand the worker (e.g. one returned by write_artifact or named in a task result). Each must already exist; the worker loads them with read_artifact. Optional, type=task only.",
                    "items": { "type": "string" }
                },
                "items": {
                    "type": "array",
                    "description": "Steps to run concurrently. Required when type=parallel.",
                    "items": { "type": "object" }
                },
                "steps": {
                    "type": "array",
                    "description": "Sequential steps in a sub-chain. Required when type=chain.",
                    "items": { "type": "object" }
                }
            }
        });

        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Decompose the user's query into an ordered sequence of steps for \
                orchestrated execution. Steps run sequentially by default. Use \
                {\"parallel\": [...]} when tasks are independent. Use this for queries \
                requiring tool execution, data gathering, system inspection, or multi-step \
                analysis."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The overall goal this plan addresses"
                    },
                    "steps": {
                        "type": "array",
                        "description": "Ordered steps to execute. Each step runs after the previous one completes. Use {\"parallel\": [...]} to run independent steps concurrently.",
                        "items": step_schema
                    },
                    "routing_rationale": {
                        "type": "string",
                        "description": "Brief explanation of why this query requires orchestration"
                    },
                    "planning_summary": {
                        "type": "string",
                        "minLength": 1,
                        "description": "REQUIRED. Summarize the plan in natural language: what steps will run, in what order, and what the expected outcome is."
                    }
                },
                "required": ["goal", "steps", "routing_rationale", "planning_summary"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut guard = self.decision.lock().await;
        if guard.is_some() {
            return Ok(CreatePlanOutput {
                status: "Error: You have already selected a routing action. Only one routing tool may be called per turn.".to_string(),
            });
        }

        // Validate the shape now, while the model can still fix it: a plan
        // that cannot be flattened must not be recorded as a routing decision.
        if let Err(e) = flatten_steps(&args.steps) {
            return Ok(CreatePlanOutput {
                status: format!(
                    "Error: {e}. The top-level `steps` list already runs in order, so use \
                     `chain` only inside a `parallel` group, and call create_plan again."
                ),
            });
        }

        if let Err(e) = self.validate_artifacts(&args.steps).await {
            return Ok(CreatePlanOutput {
                status: format!("Error: {e} Fix the task `artifacts` and call create_plan again."),
            });
        }

        let step_count = count_leaf_steps(&args.steps);
        *guard = Some(PlanningResponse::StepsPlan {
            goal: args.goal,
            steps: args.steps,
            routing_rationale: args.routing_rationale,
            planning_summary: args.planning_summary,
        });
        Ok(CreatePlanOutput {
            status: format!("Plan created with {} steps.", step_count),
        })
    }
}

impl CreatePlanTool {
    /// Check that every artifact attached to a task exists in the current run,
    /// so a worker is never sent to read a file that isn't there.
    async fn validate_artifacts(&self, steps: &[StepInput]) -> Result<(), String> {
        let mut requested = Vec::new();
        collect_artifacts(steps, &mut requested);
        if requested.is_empty() {
            return Ok(());
        }
        let Some(persistence) = &self.persistence else {
            return Err("Artifacts are unavailable in this run, so tasks cannot attach them.                         Embed the content in the task description instead."
                .to_string());
        };

        let persistence = persistence.lock().await;
        let mut missing = Vec::new();
        for name in requested {
            let exists = match persistence.artifact_path(name) {
                Ok(path) => tokio::fs::try_exists(&path).await.unwrap_or(false),
                Err(_) => false,
            };
            if !exists && !missing.contains(&name) {
                missing.push(name);
            }
        }
        if missing.is_empty() {
            return Ok(());
        }

        let available = persistence.list_artifacts().await.unwrap_or_default();
        let available = if available.is_empty() {
            "none".to_string()
        } else {
            available.join(", ")
        };
        Err(format!(
            "Artifact(s) not found in this run: {}. Available artifacts: {available}. \
             Write new content with write_artifact first and use the filename it returns.",
            missing.join(", ")
        ))
    }
}

/// Collect the artifact filenames attached to every leaf task, in step order.
fn collect_artifacts<'a>(steps: &'a [StepInput], out: &mut Vec<&'a str>) {
    for step in steps {
        match step {
            StepInput::LeafTask { artifacts, .. } => {
                out.extend(artifacts.iter().map(String::as_str));
            }
            StepInput::ParallelGroup { items } => collect_artifacts(items, out),
            StepInput::SubChain { steps } => collect_artifacts(steps, out),
        }
    }
}

/// Count the number of leaf tasks in a step tree (for status messages).
fn count_leaf_steps(steps: &[StepInput]) -> usize {
    steps.iter().map(count_one).sum()
}

fn count_one(step: &StepInput) -> usize {
    match step {
        StepInput::LeafTask { .. } => 1,
        StepInput::ParallelGroup { items } => count_leaf_steps(items),
        StepInput::SubChain { steps } => count_leaf_steps(steps),
    }
}

// ============================================================================
// RequestClarificationTool
// ============================================================================

/// Routing tool: request clarification from the user for ambiguous queries.
#[derive(Clone)]
pub struct RequestClarificationTool {
    pub(crate) decision: RoutingDecision,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RequestClarificationArgs {
    /// The clarification question to ask the user.
    pub question: String,
    /// Optional suggested options for the user to choose from.
    #[serde(default)]
    pub options: Option<Vec<String>>,
    /// Why clarification is needed.
    pub routing_rationale: String,
}

#[derive(Debug, Serialize)]
pub struct RequestClarificationOutput {
    pub status: String,
}

impl std::fmt::Display for RequestClarificationOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.status)
    }
}

impl Tool for RequestClarificationTool {
    const NAME: &'static str = "request_clarification";

    type Error = Infallible;
    type Args = RequestClarificationArgs;
    type Output = RequestClarificationOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Request clarification from the user when the query is genuinely \
                ambiguous. Use sparingly — prefer create_plan when a reasonable interpretation \
                exists."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The clarification question to ask the user"
                    },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional suggested choices for the user"
                    },
                    "routing_rationale": {
                        "type": "string",
                        "description": "Brief explanation of why clarification is needed"
                    }
                },
                "required": ["question", "routing_rationale"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut guard = self.decision.lock().await;
        if guard.is_some() {
            return Ok(RequestClarificationOutput {
                status: "Error: You have already selected a routing action. Only one routing tool may be called per turn.".to_string(),
            });
        }
        *guard = Some(PlanningResponse::Clarification {
            question: args.question,
            options: args.options,
            routing_rationale: args.routing_rationale,
        });
        Ok(RequestClarificationOutput {
            status: "Clarification request recorded.".to_string(),
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
    async fn test_respond_directly_stores_decision() {
        let toolset = RoutingToolSet::new();
        let result = toolset
            .respond_directly
            .call(RespondDirectlyArgs {
                response: "42".to_string(),
                routing_rationale: "Simple arithmetic".to_string(),
                summary: None,
            })
            .await
            .unwrap();

        assert!(result.status.contains("recorded"));

        let decision = toolset.decision.lock().await;
        match decision.as_ref().unwrap() {
            PlanningResponse::Direct {
                response,
                routing_rationale,
                ..
            } => {
                assert_eq!(response, "42");
                assert_eq!(routing_rationale, "Simple arithmetic");
            }
            other => panic!("Expected Direct, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_create_plan_rejects_unflattenable_steps() {
        let toolset = RoutingToolSet::new();
        let leaf = |task: &str| StepInput::LeafTask {
            task: task.to_string(),
            worker: Some("operations".to_string()),
            artifacts: Vec::new(),
        };
        // chain > chain > parallel: one level deeper than flatten_steps allows.
        let result = toolset
            .create_plan
            .call(CreatePlanArgs {
                goal: "Nested".to_string(),
                steps: vec![StepInput::SubChain {
                    steps: vec![
                        leaf("draft"),
                        StepInput::SubChain {
                            steps: vec![
                                leaf("review"),
                                StepInput::ParallelGroup {
                                    items: vec![leaf("verify a"), leaf("verify b")],
                                },
                            ],
                        },
                    ],
                }],
                routing_rationale: "r".to_string(),
                planning_summary: "s".to_string(),
            })
            .await
            .unwrap();

        assert!(result.status.starts_with("Error:"), "{}", result.status);
        assert!(result.status.contains("nesting depth"), "{}", result.status);
        assert!(
            toolset.decision.lock().await.is_none(),
            "a rejected plan must not be recorded as the routing decision"
        );
    }

    fn plan_with_artifacts(artifacts: &[&str]) -> CreatePlanArgs {
        CreatePlanArgs {
            goal: "Review the draft".to_string(),
            steps: vec![
                StepInput::LeafTask {
                    task: "Gather context".to_string(),
                    worker: Some("operations".to_string()),
                    artifacts: Vec::new(),
                },
                StepInput::ParallelGroup {
                    items: vec![StepInput::LeafTask {
                        task: "Review the draft".to_string(),
                        worker: Some("reviewer".to_string()),
                        artifacts: artifacts.iter().map(|a| a.to_string()).collect(),
                    }],
                },
            ],
            routing_rationale: "r".to_string(),
            planning_summary: "s".to_string(),
        }
    }

    async fn persistence_with_draft() -> (Arc<Mutex<ExecutionPersistence>>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        persistence
            .write_coordinator_artifact("draft.md", "# Draft")
            .await
            .unwrap();
        (Arc::new(Mutex::new(persistence)), temp_dir)
    }

    #[tokio::test]
    async fn test_create_plan_accepts_existing_artifacts() {
        let (persistence, _dir) = persistence_with_draft().await;
        let toolset = RoutingToolSet::new().with_persistence(persistence);

        let result = toolset
            .create_plan
            .call(plan_with_artifacts(&["coordinator-draft.md"]))
            .await
            .unwrap();

        assert!(
            result.status.starts_with("Plan created"),
            "{}",
            result.status
        );
        let decision = toolset.decision.lock().await;
        let Some(PlanningResponse::StepsPlan { steps, .. }) = decision.as_ref() else {
            panic!("expected StepsPlan, got {:?}", decision);
        };
        let tasks = flatten_steps(steps).unwrap();
        assert_eq!(tasks[1].artifacts, vec!["coordinator-draft.md".to_string()]);
    }

    #[tokio::test]
    async fn test_create_plan_rejects_missing_artifacts() {
        let (persistence, _dir) = persistence_with_draft().await;
        let toolset = RoutingToolSet::new().with_persistence(persistence);

        let result = toolset
            .create_plan
            .call(plan_with_artifacts(&[
                "coordinator-draft.md",
                "draft.md",
                "../../etc/passwd",
            ]))
            .await
            .unwrap();

        assert!(result.status.starts_with("Error:"), "{}", result.status);
        assert!(
            result
                .status
                .contains("not found in this run: draft.md, ../../etc/passwd."),
            "{}",
            result.status
        );
        assert!(
            result
                .status
                .contains("Available artifacts: coordinator-draft.md"),
            "{}",
            result.status
        );
        assert!(
            toolset.decision.lock().await.is_none(),
            "a plan with missing artifacts must not be recorded"
        );
    }

    #[tokio::test]
    async fn test_create_plan_rejects_artifacts_without_persistence() {
        let toolset = RoutingToolSet::new();

        let result = toolset
            .create_plan
            .call(plan_with_artifacts(&["coordinator-draft.md"]))
            .await
            .unwrap();

        assert!(
            result.status.contains("Artifacts are unavailable"),
            "{}",
            result.status
        );
        assert!(toolset.decision.lock().await.is_none());
    }

    #[tokio::test]
    async fn test_create_plan_stores_decision() {
        let toolset = RoutingToolSet::new();
        let result = toolset
            .create_plan
            .call(CreatePlanArgs {
                goal: "Investigate logs".to_string(),
                steps: vec![StepInput::LeafTask {
                    task: "Fetch recent logs".to_string(),
                    worker: Some("operations".to_string()),
                    artifacts: Vec::new(),
                }],
                routing_rationale: "Requires tool execution".to_string(),
                planning_summary: "Fetch and analyze recent logs".to_string(),
            })
            .await
            .unwrap();

        assert!(result.status.contains("1 steps"));

        let decision = toolset.decision.lock().await;
        match decision.as_ref().unwrap() {
            PlanningResponse::StepsPlan {
                goal,
                steps,
                routing_rationale,
                planning_summary,
            } => {
                assert_eq!(goal, "Investigate logs");
                assert_eq!(steps.len(), 1);
                assert_eq!(routing_rationale, "Requires tool execution");
                assert_eq!(planning_summary, "Fetch and analyze recent logs");
            }
            other => panic!("Expected StepsPlan, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_request_clarification_stores_decision() {
        let toolset = RoutingToolSet::new();
        let result = toolset
            .request_clarification
            .call(RequestClarificationArgs {
                question: "Which service?".to_string(),
                options: Some(vec!["API".to_string(), "Worker".to_string()]),
                routing_rationale: "Ambiguous service reference".to_string(),
            })
            .await
            .unwrap();

        assert!(result.status.contains("recorded"));

        let decision = toolset.decision.lock().await;
        match decision.as_ref().unwrap() {
            PlanningResponse::Clarification {
                question,
                options,
                routing_rationale,
            } => {
                assert_eq!(question, "Which service?");
                assert_eq!(options.as_ref().unwrap().len(), 2);
                assert_eq!(routing_rationale, "Ambiguous service reference");
            }
            other => panic!("Expected Clarification, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_double_call_rejected() {
        let toolset = RoutingToolSet::new();

        // First call succeeds
        let result = toolset
            .respond_directly
            .call(RespondDirectlyArgs {
                response: "First".to_string(),
                routing_rationale: "Test".to_string(),
                summary: None,
            })
            .await
            .unwrap();
        assert!(result.status.contains("recorded"));

        // Second call (different tool) is rejected
        let result = toolset
            .create_plan
            .call(CreatePlanArgs {
                goal: "test".to_string(),
                steps: vec![],
                routing_rationale: "test".to_string(),
                planning_summary: "test".to_string(),
            })
            .await
            .unwrap();
        assert!(result.status.contains("already selected"));

        // Original decision preserved
        let decision = toolset.decision.lock().await;
        assert!(matches!(
            decision.as_ref().unwrap(),
            PlanningResponse::Direct { .. }
        ));
    }

    #[tokio::test]
    async fn test_clarification_without_options() {
        let toolset = RoutingToolSet::new();
        toolset
            .request_clarification
            .call(RequestClarificationArgs {
                question: "What do you mean?".to_string(),
                options: None,
                routing_rationale: "Vague query".to_string(),
            })
            .await
            .unwrap();

        let decision = toolset.decision.lock().await;
        match decision.as_ref().unwrap() {
            PlanningResponse::Clarification { options, .. } => {
                assert!(options.is_none());
            }
            other => panic!("Expected Clarification, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_tool_definitions() {
        let toolset = RoutingToolSet::new();

        let def = toolset.respond_directly.definition("".to_string()).await;
        assert_eq!(def.name, "respond_directly");
        assert!(def.description.contains("directly"));

        let def = toolset.create_plan.definition("".to_string()).await;
        assert_eq!(def.name, "create_plan");
        assert!(def.description.contains("ordered sequence"));

        let def = toolset
            .request_clarification
            .definition("".to_string())
            .await;
        assert_eq!(def.name, "request_clarification");
        assert!(def.description.contains("clarification"));
    }
}
