//! Execution persistence for orchestration observability.
//!
//! Writes detailed execution artifacts to disk asynchronously for debugging,
//! analysis, and future retry intelligence. Supports iteration tracking for
//! replanning scenarios.
//!
//! ## Directory Structure
//!
//! ```text
//! {base_path}/
//! ├── latest -> {run_id}/              # Symlink to most recent run
//! └── {run_id}/
//!     ├── artifacts/                   # Run-level result artifacts
//!     │   └── task-0-result.txt
//!     └── iteration-{n}/              # One flat dir per iteration
//!         ├── plan.json
//!         ├── summary.json
//!         ├── planning.prompt.txt
//!         ├── planning.response.txt
//!         ├── task-{id}.attempt-{n}.prompt.txt
//!         ├── task-{id}.attempt-{n}.response.txt
//!         ├── task-{id}.attempt-{n}.tool-calls.json
//!         ├── task-{id}.attempt-{n}.result.json
//!         ├── synthesis.prompt.txt
//!         ├── synthesis.response.txt
//!         ├── evaluation.prompt.txt
//!         ├── evaluation.response.txt
//!         └── evaluation.result.json
//! ```

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use tokio::fs;

use super::types::Plan;

/// A single tool call made during task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Tool name
    pub tool: String,
    /// Arguments passed to the tool
    pub arguments: serde_json::Value,
    /// Why this tool was called
    pub reasoning: String,
    /// Tool output (may be truncated for large outputs)
    pub output: Option<String>,
    /// Error if tool call failed
    pub error: Option<String>,
    /// Duration in milliseconds
    pub duration_ms: u64,
}

/// Summary of a worker's execution for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskExecutionRecord {
    /// Task ID
    pub task_id: usize,
    /// Task description
    pub description: String,
    /// Attempt number (1-indexed)
    pub attempt: usize,
    /// Worker's approach/reasoning
    pub approach: String,
    /// Tools called during execution
    pub tool_calls: Vec<ToolCallRecord>,
    /// Final result
    pub result: Option<String>,
    /// Error if task failed
    pub error: Option<String>,
    /// Duration in milliseconds
    pub duration_ms: u64,
    /// Worker's confidence level
    pub confidence: Option<String>,
    /// Notes for orchestrator (retry hints, blockers, etc.)
    pub orchestrator_notes: Option<String>,
}

/// Manages execution artifact persistence (async).
#[derive(Clone)]
pub struct ExecutionPersistence {
    base_path: PathBuf,
    run_id: String,
    current_iteration: usize,
    enabled: bool,
}

impl ExecutionPersistence {
    /// Create new persistence manager with unique run ID.
    ///
    /// Creates the run directory and a `latest` symlink.
    pub async fn new<P: AsRef<Path>>(base_path: P) -> io::Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();

        // Generate unique run ID
        let run_id = uuid::Uuid::new_v4().to_string();
        let run_path = base_path.join(&run_id);

        fs::create_dir_all(&run_path).await?;

        // Create symlink to latest run (best effort, ignore errors)
        let latest_path = base_path.join("latest");
        let _ = tokio::fs::remove_file(&latest_path).await;

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let _ = tokio::task::spawn_blocking({
                let run_id = run_id.clone();
                let latest_path = latest_path.clone();
                move || symlink(&run_id, latest_path)
            })
            .await;
        }

        tracing::info!(
            "🗂️ Execution persistence initialized: {}",
            run_path.display()
        );

        Ok(Self {
            base_path: run_path,
            run_id,
            current_iteration: 0,
            enabled: true,
        })
    }

    /// Create a disabled persistence manager (no-op writes).
    pub fn disabled() -> Self {
        Self {
            base_path: PathBuf::new(),
            run_id: String::new(),
            current_iteration: 0,
            enabled: false,
        }
    }

    /// Get the run ID for this execution.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Get current iteration number.
    pub fn current_iteration(&self) -> usize {
        self.current_iteration
    }

    /// Start a new iteration (for replanning).
    pub fn start_new_iteration(&mut self) -> usize {
        self.current_iteration += 1;
        self.current_iteration
    }

    /// Get iteration directory path (flat, directly under run dir).
    fn iteration_path(&self) -> PathBuf {
        self.base_path
            .join(format!("iteration-{}", self.current_iteration))
    }

    /// Build a dot-namespaced filename for a task attempt artifact.
    fn task_attempt_filename(&self, task_id: usize, attempt: usize, suffix: &str) -> String {
        format!("task-{}.attempt-{}.{}", task_id, attempt, suffix)
    }

    /// Write the plan created by coordinator.
    pub async fn write_plan(&self, plan: &Plan) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        let plan_path = iter_path.join("plan.json");
        let json = serde_json::to_string_pretty(plan)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&plan_path, json).await?;

        tracing::debug!("Written plan to: {}", plan_path.display());
        Ok(plan_path)
    }

    /// Write planning phase artifacts (coordinator prompt/response).
    pub async fn write_planning_phase(&self, prompt: &str, response: &str) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        fs::write(iter_path.join("planning.prompt.txt"), prompt).await?;
        fs::write(iter_path.join("planning.response.txt"), response).await?;

        Ok(iter_path)
    }

    /// Write worker task execution artifacts.
    pub async fn write_task_execution(
        &self,
        task_id: usize,
        attempt: usize,
        prompt: &str,
        response: &str,
        record: &TaskExecutionRecord,
    ) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        // Write prompt and response with namespaced filenames
        let prompt_file = self.task_attempt_filename(task_id, attempt, "prompt.txt");
        let response_file = self.task_attempt_filename(task_id, attempt, "response.txt");
        fs::write(iter_path.join(&prompt_file), prompt).await?;
        fs::write(iter_path.join(&response_file), response).await?;

        // Write tool calls separately for easy inspection
        if !record.tool_calls.is_empty() {
            let tool_json = serde_json::to_string_pretty(&record.tool_calls)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let tool_file = self.task_attempt_filename(task_id, attempt, "tool-calls.json");
            fs::write(iter_path.join(&tool_file), tool_json).await?;
        }

        // Write full execution record
        let record_json = serde_json::to_string_pretty(record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let result_file = self.task_attempt_filename(task_id, attempt, "result.json");
        fs::write(iter_path.join(&result_file), record_json).await?;

        tracing::debug!(
            "Written task execution to: {}/{}",
            iter_path.display(),
            prompt_file
        );
        Ok(iter_path)
    }

    /// Write synthesis phase artifacts.
    pub async fn write_synthesis(&self, prompt: &str, response: &str) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        fs::write(iter_path.join("synthesis.prompt.txt"), prompt).await?;
        fs::write(iter_path.join("synthesis.response.txt"), response).await?;

        Ok(iter_path)
    }

    /// Write evaluation phase artifacts.
    ///
    /// Persists the evaluation prompt, raw LLM response, and parsed result.
    pub async fn write_evaluation(
        &self,
        prompt: &str,
        response: &str,
        result: &super::types::EvaluationResult,
    ) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        fs::write(iter_path.join("evaluation.prompt.txt"), prompt).await?;
        fs::write(iter_path.join("evaluation.response.txt"), response).await?;

        let result_json = serde_json::to_string_pretty(result)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(iter_path.join("evaluation.result.json"), result_json).await?;

        tracing::debug!("Written evaluation to: {}", iter_path.display());
        Ok(iter_path)
    }

    /// Write iteration summary (quality score, replan decision, etc.).
    pub async fn write_iteration_summary(
        &self,
        iteration: usize,
        quality_score: f32,
        threshold: f32,
        will_replan: bool,
    ) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let summary = serde_json::json!({
            "iteration": iteration,
            "quality_score": quality_score,
            "threshold": threshold,
            "will_replan": will_replan,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        let summary_path = iter_path.join("summary.json");
        let json = serde_json::to_string_pretty(&summary)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&summary_path, json).await?;

        Ok(summary_path)
    }

    /// Get relative path for logging.
    pub fn relative_path(&self, task_id: usize, attempt: usize) -> String {
        self.task_attempt_filename(task_id, attempt, "*")
    }

    // ========================================================================
    // Result Artifact Methods
    // ========================================================================

    /// Directory for result artifacts (run-level, not per-iteration).
    fn artifacts_path(&self) -> PathBuf {
        self.base_path.join("artifacts")
    }

    /// Write a large result to an artifact file.
    ///
    /// Returns the artifact filename (not the full path) for reference in summaries.
    pub async fn write_result_artifact(&self, task_id: usize, result: &str) -> io::Result<String> {
        if !self.enabled {
            return Ok(String::new());
        }

        let artifacts_dir = self.artifacts_path();
        fs::create_dir_all(&artifacts_dir).await?;

        let filename = format!("task-{}-result.txt", task_id);
        let artifact_path = artifacts_dir.join(&filename);
        fs::write(&artifact_path, result).await?;

        tracing::debug!(
            "Written result artifact ({} chars) to: {}",
            result.len(),
            artifact_path.display()
        );
        Ok(filename)
    }

    /// Read an artifact file by filename.
    ///
    /// Validates the filename to prevent path traversal.
    pub async fn read_artifact(&self, filename: &str) -> io::Result<String> {
        if !self.enabled {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Persistence is disabled",
            ));
        }

        // Path traversal check
        if filename.contains('/')
            || filename.contains('\\')
            || filename.contains("..")
            || filename.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid artifact filename",
            ));
        }

        let artifact_path = self.artifacts_path().join(filename);
        fs::read_to_string(&artifact_path).await
    }

    /// List all artifact filenames.
    pub async fn list_artifacts(&self) -> io::Result<Vec<String>> {
        if !self.enabled {
            return Ok(Vec::new());
        }

        let artifacts_dir = self.artifacts_path();
        if !artifacts_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = fs::read_dir(&artifacts_dir).await?;
        let mut filenames = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                filenames.push(name.to_string());
            }
        }
        filenames.sort();
        Ok(filenames)
    }

    /// Append a tool call record to the current task's execution.
    ///
    /// This is called by PersistenceWrapper during tool execution.
    /// Tool calls are appended to a running list, not overwritten.
    pub async fn append_tool_call(
        &self,
        task_id: usize,
        attempt: usize,
        record: &ToolCallRecord,
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        let tool_file = self.task_attempt_filename(task_id, attempt, "tool-calls.json");
        let tool_calls_path = iter_path.join(&tool_file);

        // Read existing tool calls or start fresh
        let mut tool_calls: Vec<ToolCallRecord> = if tool_calls_path.exists() {
            let content = fs::read_to_string(&tool_calls_path).await?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Append new record
        tool_calls.push(record.clone());

        // Write back
        let json = serde_json::to_string_pretty(&tool_calls)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tool_calls_path, json).await?;

        tracing::debug!(
            "Appended tool call to: {} (total: {})",
            tool_calls_path.display(),
            tool_calls.len()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_persistence_creation() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory")).await;
        assert!(persistence.is_ok());
    }

    #[tokio::test]
    async fn test_iteration_tracking() {
        let temp_dir = TempDir::new().unwrap();
        let mut persistence = ExecutionPersistence::new(temp_dir.path().join("memory"))
            .await
            .unwrap();

        assert_eq!(persistence.current_iteration(), 0);
        assert_eq!(persistence.start_new_iteration(), 1);
        assert_eq!(persistence.current_iteration(), 1);
    }

    #[tokio::test]
    async fn test_disabled_persistence() {
        let persistence = ExecutionPersistence::disabled();
        assert!(!persistence.enabled);

        // All writes should succeed but do nothing
        let result = persistence.write_plan(&Plan::new("test")).await;
        assert!(result.is_ok());
    }

    // ========================================================================
    // Result Artifact Tests
    // ========================================================================

    #[tokio::test]
    async fn test_write_and_read_artifact() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"))
            .await
            .unwrap();

        let filename = persistence
            .write_result_artifact(0, "full result content")
            .await
            .unwrap();
        assert_eq!(filename, "task-0-result.txt");

        let content = persistence.read_artifact(&filename).await.unwrap();
        assert_eq!(content, "full result content");
    }

    #[tokio::test]
    async fn test_list_artifacts() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"))
            .await
            .unwrap();

        // Initially empty
        let artifacts = persistence.list_artifacts().await.unwrap();
        assert!(artifacts.is_empty());

        // Write two artifacts
        persistence
            .write_result_artifact(0, "result 0")
            .await
            .unwrap();
        persistence
            .write_result_artifact(1, "result 1")
            .await
            .unwrap();

        let artifacts = persistence.list_artifacts().await.unwrap();
        assert_eq!(artifacts.len(), 2);
        assert!(artifacts.contains(&"task-0-result.txt".to_string()));
        assert!(artifacts.contains(&"task-1-result.txt".to_string()));
    }

    #[tokio::test]
    async fn test_read_artifact_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"))
            .await
            .unwrap();

        let result = persistence.read_artifact("nonexistent.txt").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn test_read_artifact_path_traversal() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"))
            .await
            .unwrap();

        // All path traversal attempts should fail
        for bad_name in &["../secret.txt", "foo/bar.txt", "..\\secret", ""] {
            let result = persistence.read_artifact(bad_name).await;
            assert!(result.is_err(), "Should reject: {:?}", bad_name);
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[tokio::test]
    async fn test_disabled_persistence_artifacts() {
        let persistence = ExecutionPersistence::disabled();

        // Write returns empty string
        let filename = persistence
            .write_result_artifact(0, "content")
            .await
            .unwrap();
        assert!(filename.is_empty());

        // Read fails
        let result = persistence.read_artifact("task-0-result.txt").await;
        assert!(result.is_err());

        // List returns empty
        let artifacts = persistence.list_artifacts().await.unwrap();
        assert!(artifacts.is_empty());
    }
}
