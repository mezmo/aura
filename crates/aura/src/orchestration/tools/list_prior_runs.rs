//! Coordinator-only inspection tool for discovering prior runs in the session.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::orchestration::persistence::{ExecutionPersistence, RunStatus, load_session_manifests};

const MAX_PRIOR_RUNS: usize = 50;

/// Lists all prior runs in the current session with metadata.
#[derive(Clone)]
pub struct ListPriorRunsTool {
    persistence: Arc<Mutex<ExecutionPersistence>>,
    memory_dir: PathBuf,
}

impl ListPriorRunsTool {
    pub fn new(persistence: Arc<Mutex<ExecutionPersistence>>, memory_dir: PathBuf) -> Self {
        Self {
            persistence,
            memory_dir,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ListPriorRunsArgs {}

#[derive(Debug, Serialize)]
pub struct RunInfo {
    pub run_id: String,
    pub timestamp: String,
    pub goal: String,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    pub iteration_count: usize,
    pub task_count: usize,
    pub artifact_count: usize,
}

#[derive(Debug, Serialize)]
pub struct ListPriorRunsOutput {
    pub count: usize,
    pub runs: Vec<RunInfo>,
}

impl Tool for ListPriorRunsTool {
    const NAME: &'static str = "list_prior_runs";

    type Error = Infallible;
    type Args = ListPriorRunsArgs;
    type Output = ListPriorRunsOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "List all prior runs in the current session. Returns run metadata \
                 including run_id, goal, outcome, and artifact counts. Use run_id values \
                 with read_artifact for cross-run artifact access."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let persistence = self.persistence.lock().await;
        let session_id = match persistence.session_id() {
            Some(sid) => sid.to_string(),
            None => {
                tracing::warn!("list_prior_runs called without session_id — returning empty");
                return Ok(ListPriorRunsOutput {
                    count: 0,
                    runs: Vec::new(),
                });
            }
        };
        let run_id = persistence.run_id().to_string();
        drop(persistence);

        let manifests =
            load_session_manifests(&self.memory_dir, &session_id, &run_id, MAX_PRIOR_RUNS)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("list_prior_runs failed to load manifests: {e}");
                    Vec::new()
                });

        tracing::info!(
            "Coordinator called list_prior_runs ({} prior run(s) found)",
            manifests.len()
        );

        let runs: Vec<RunInfo> = manifests
            .into_iter()
            .map(|m| RunInfo {
                run_id: m.run_id,
                timestamp: m.timestamp,
                goal: m.goal,
                status: m.status,
                outcome: m.outcome,
                iteration_count: m.iterations,
                task_count: m.task_summaries.len(),
                artifact_count: m.artifact_paths.len(),
            })
            .collect();

        let count = runs.len();
        Ok(ListPriorRunsOutput { count, runs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::persistence::RunManifest;
    use tempfile::TempDir;
    use tokio::fs;

    async fn setup_persistence_with_session(
        temp_dir: &TempDir,
        session_id: &str,
    ) -> Arc<Mutex<ExecutionPersistence>> {
        let persistence =
            ExecutionPersistence::new(temp_dir.path(), Some(session_id.to_string()))
                .await
                .unwrap();
        Arc::new(Mutex::new(persistence))
    }

    async fn write_manifest(base: &std::path::Path, session_id: &str, manifest: &RunManifest) {
        let run_dir = base.join(session_id).join(&manifest.run_id);
        fs::create_dir_all(&run_dir).await.unwrap();
        let content = serde_json::to_string_pretty(manifest).unwrap();
        fs::write(run_dir.join("manifest.json"), content)
            .await
            .unwrap();
    }

    fn make_manifest(run_id: &str, goal: &str, task_count: usize) -> RunManifest {
        use crate::orchestration::persistence::TaskSummary;
        use crate::orchestration::types::TaskStatus;

        RunManifest {
            run_id: run_id.to_string(),
            session_id: Some("test-session".to_string()),
            timestamp: format!("2026-05-01T00:0{}:00Z", task_count),
            goal: goal.to_string(),
            status: RunStatus::Success,
            iterations: 1,
            routing_mode: None,
            outcome: Some("All tasks completed".to_string()),
            response_summary: None,
            task_summaries: (0..task_count)
                .map(|i| TaskSummary {
                    task_id: i,
                    description: format!("Task {i}"),
                    status: TaskStatus::Complete,
                    worker: Some("test-worker".to_string()),
                    result_preview: None,
                    confidence: None,
                    failure_category: None,
                    error: None,
                    error_context: None,
                    tool_trace: Vec::new(),
                    artifacts: Vec::new(),
                })
                .collect(),
            artifact_paths: vec!["artifacts/result.txt".to_string()],
        }
    }

    #[tokio::test]
    async fn test_list_prior_runs_empty_session() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = setup_persistence_with_session(&temp_dir, "empty-session").await;
        let tool = ListPriorRunsTool::new(persistence, temp_dir.path().to_path_buf());
        let result = tool.call(ListPriorRunsArgs {}).await.unwrap();
        assert_eq!(result.count, 0);
        assert!(result.runs.is_empty());
    }

    #[tokio::test]
    async fn test_list_prior_runs_with_manifests() {
        let temp_dir = TempDir::new().unwrap();
        let session_id = "test-session";
        let persistence = setup_persistence_with_session(&temp_dir, session_id).await;

        let m1 = make_manifest("run-aaa", "Compute sum of 2+3", 2);
        let m2 = make_manifest("run-bbb", "Calculate mean of [1,2,3]", 3);
        write_manifest(temp_dir.path(), session_id, &m1).await;
        write_manifest(temp_dir.path(), session_id, &m2).await;

        let tool = ListPriorRunsTool::new(persistence, temp_dir.path().to_path_buf());
        let result = tool.call(ListPriorRunsArgs {}).await.unwrap();

        assert_eq!(result.count, 2);
        let run_ids: Vec<&str> = result.runs.iter().map(|r| r.run_id.as_str()).collect();
        assert!(run_ids.contains(&"run-aaa"));
        assert!(run_ids.contains(&"run-bbb"));

        let run_a = result.runs.iter().find(|r| r.run_id == "run-aaa").unwrap();
        assert_eq!(run_a.task_count, 2);
        assert_eq!(run_a.artifact_count, 1);
        assert_eq!(run_a.goal, "Compute sum of 2+3");
        assert_eq!(run_a.status, RunStatus::Success);
        assert_eq!(run_a.outcome.as_deref(), Some("All tasks completed"));

        let run_b = result.runs.iter().find(|r| r.run_id == "run-bbb").unwrap();
        assert_eq!(run_b.task_count, 3);
    }

    #[tokio::test]
    async fn test_list_prior_runs_no_session_id() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::disabled();
        let persistence = Arc::new(Mutex::new(persistence));
        let tool = ListPriorRunsTool::new(persistence, temp_dir.path().to_path_buf());
        let result = tool.call(ListPriorRunsArgs {}).await.unwrap();
        assert_eq!(result.count, 0);
        assert!(result.runs.is_empty());
    }

    #[tokio::test]
    async fn test_list_prior_runs_definition() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = setup_persistence_with_session(&temp_dir, "def-session").await;
        let tool = ListPriorRunsTool::new(persistence, temp_dir.path().to_path_buf());
        let def = tool.definition("".to_string()).await;
        assert_eq!(def.name, "list_prior_runs");
        assert!(def.description.contains("prior runs"));
        assert!(def.description.contains("run_id"));
    }
}
