//! Execution persistence for orchestration observability.
//!
//! Writes detailed execution artifacts to disk asynchronously for debugging,
//! analysis, and future retry intelligence. Supports iteration tracking for
//! replanning scenarios.
//!
//! ## Directory Structure
//!
//! With session_id (web server path):
//! ```text
//! {base_path}/{session_id}/
//! ├── latest -> {run_id}/              # Symlink to most recent run in session
//! └── {run_id}/
//!     ├── manifest.json                # Typed run manifest (RunManifest)
//!     ├── artifacts/                   # Run-level result artifacts
//!     │   └── task-0-result.txt
//!     └── iteration-{n}/              # One flat dir per iteration
//!         ├── plan.json
//!         ├── ...
//! ```
//!
//! Without session_id (CLI/test path):
//! ```text
//! {base_path}/
//! ├── latest -> {run_id}/
//! └── {run_id}/
//!     ├── manifest.json
//!     ├── artifacts/
//!     └── iteration-{n}/
//! ```

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use tokio::fs;

use super::events::RoutingMode;
use super::types::{Plan, TaskStatus};

// ============================================================================
// Run Manifest Types
// ============================================================================

/// Typed manifest written at the end of each orchestration run.
///
/// This is the "typed metadata, untyped blobs" pattern: the manifest is a
/// structured index into the run's artifacts. Phase 2 uses manifests for
/// cross-turn context without reading raw artifact files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    /// Unique run identifier.
    pub run_id: String,
    /// Session that owns this run (None for CLI/test).
    pub session_id: Option<String>,
    /// ISO 8601 timestamp of run completion.
    pub timestamp: String,
    /// The goal from the orchestration plan.
    pub goal: String,
    /// Overall run outcome.
    pub status: RunStatus,
    /// Number of plan-execute-evaluate cycles.
    pub iterations: usize,
    /// Final quality evaluation score (if evaluation ran).
    pub quality_score: Option<f32>,
    /// How the coordinator routed this query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_mode: Option<RoutingMode>,
    /// Summary of each task in the plan.
    pub task_summaries: Vec<TaskSummary>,
    /// Relative paths to large artifact files.
    pub artifact_paths: Vec<String>,
}

/// Summary of a single task for the run manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    /// Task ID within the plan.
    pub task_id: usize,
    /// Human-readable task description.
    pub description: String,
    /// Final task status.
    pub status: TaskStatus,
    /// Assigned worker name (if any).
    pub worker: Option<String>,
    /// First ~200 chars of the result (for quick scanning).
    pub result_preview: Option<String>,
}

/// Overall outcome of an orchestration run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// All tasks completed successfully and quality threshold met.
    Success,
    /// Run completed but some tasks failed or quality threshold not met.
    PartialSuccess,
    /// Run failed entirely.
    Failed,
}

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
    /// Tools called during execution (always empty — tool calls are persisted
    /// incrementally to separate `*.tool-calls.json` via `append_tool_call()`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    session_id: Option<String>,
    current_iteration: usize,
    enabled: bool,
}

impl ExecutionPersistence {
    /// Create new persistence manager with unique run ID.
    ///
    /// Creates the run directory and a `latest` symlink.
    ///
    /// When `session_id` is provided, the directory structure becomes
    /// `{base_path}/{session_id}/{run_id}/...`, grouping runs by session.
    /// Without a session_id, the flat `{base_path}/{run_id}/...` layout is used.
    pub async fn new<P: AsRef<Path>>(base_path: P, session_id: Option<String>) -> io::Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();

        // Validate session_id to prevent path traversal
        if let Some(ref sid) = session_id
            && (sid.is_empty() || sid.contains('/') || sid.contains('\\') || sid.contains(".."))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid session_id for persistence path: {:?}", sid),
            ));
        }

        // Compute effective base: with session namespace or flat
        let effective_base = if let Some(ref sid) = session_id {
            base_path.join(sid)
        } else {
            base_path.clone()
        };

        // Generate unique run ID
        let run_id = uuid::Uuid::new_v4().to_string();
        let run_path = effective_base.join(&run_id);

        fs::create_dir_all(&run_path).await?;

        // Create symlink to latest run (best effort, ignore errors)
        let latest_path = effective_base.join("latest");
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
            session_id,
            current_iteration: 1,
            enabled: true,
        })
    }

    /// Create a disabled persistence manager (no-op writes).
    pub fn disabled() -> Self {
        Self {
            base_path: PathBuf::new(),
            run_id: uuid::Uuid::new_v4().to_string(),
            session_id: None,
            current_iteration: 1,
            enabled: false,
        }
    }

    /// Get the run ID for this execution.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Get the base path for this run's artifacts.
    pub fn run_path(&self) -> &Path {
        &self.base_path
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

    // ========================================================================
    // Run Manifest
    // ========================================================================

    /// Get the session ID (if set).
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Write a typed run manifest to `{run_path}/manifest.json`.
    ///
    /// Called at the end of `run_orchestration_loop()` on both success and
    /// failure paths. The manifest serves as a structured index for Phase 2
    /// cross-turn context.
    pub async fn write_manifest(&self, manifest: &RunManifest) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let manifest_path = self.base_path.join("manifest.json");
        let json = serde_json::to_string_pretty(manifest)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&manifest_path, json).await?;

        tracing::info!("Written run manifest to: {}", manifest_path.display());
        Ok(manifest_path)
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

// ============================================================================
// Session History — Cross-Run Manifest Loading
// ============================================================================

/// Session history template loaded at compile time.
const SESSION_HISTORY_TEMPLATE: &str = include_str!("../prompts/session_history.md");

/// Load run manifests from prior runs in a session directory.
///
/// Reads `{base_path}/{session_id}/*/manifest.json`, sorts by timestamp
/// descending, excludes the current run, and returns up to `limit` manifests.
pub async fn load_session_manifests(
    base_path: &Path,
    session_id: &str,
    exclude_run_id: &str,
    limit: usize,
) -> io::Result<Vec<RunManifest>> {
    let session_dir = base_path.join(session_id);
    if !session_dir.exists() {
        return Ok(Vec::new());
    }

    let mut manifests = Vec::new();
    let mut entries = fs::read_dir(&session_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Skip the current run and the "latest" symlink
        if let Some(dir_name) = path.file_name().and_then(|n| n.to_str())
            && (dir_name == exclude_run_id || dir_name == "latest")
        {
            continue;
        }

        let manifest_path = path.join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }

        match fs::read_to_string(&manifest_path).await {
            Ok(content) => match serde_json::from_str::<RunManifest>(&content) {
                Ok(manifest) => manifests.push(manifest),
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse manifest at {}: {}",
                        manifest_path.display(),
                        e
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    "Failed to read manifest at {}: {}",
                    manifest_path.display(),
                    e
                );
            }
        }
    }

    // Sort by timestamp descending (most recent first)
    manifests.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    // Take the most recent `limit` manifests
    manifests.truncate(limit);

    Ok(manifests)
}

/// Build a session context string from prior run manifests.
///
/// Renders the `session_history.md` template with turn entries built from
/// each manifest. All static guidance lives in the template; this function
/// only fills `%%VAR%%` placeholders.
pub fn build_session_context(manifests: &[RunManifest]) -> String {
    if manifests.is_empty() {
        return String::new();
    }

    let mut turn_entries = String::new();

    // Manifests are sorted most-recent-first; number turns chronologically
    for (i, manifest) in manifests.iter().rev().enumerate() {
        let turn_num = i + 1;
        let status = format!("{:?}", manifest.status);
        let score = manifest
            .quality_score
            .map(|s| format!("{:.2}", s))
            .unwrap_or_else(|| "N/A".to_string());

        turn_entries.push_str(&format!(
            "### Turn {} ({}) — {} (quality: {})\n",
            turn_num, manifest.timestamp, status, score
        ));
        turn_entries.push_str(&format!("Goal: \"{}\"\n", manifest.goal));

        if !manifest.task_summaries.is_empty() {
            turn_entries.push_str("Tasks:\n");
            for task in &manifest.task_summaries {
                let worker = task.worker.as_deref().unwrap_or("unassigned");
                let result = match (&task.status, &task.result_preview) {
                    (TaskStatus::Complete, Some(preview)) => format!("→ \"{}\"", preview),
                    (TaskStatus::Failed, Some(preview)) => format!("→ FAILED: \"{}\"", preview),
                    (TaskStatus::Failed, None) => "→ FAILED".to_string(),
                    (status, _) => format!("→ {}", status),
                };
                turn_entries.push_str(&format!(
                    "  - Task {} [{}]: {} {}\n",
                    task.task_id, worker, task.description, result
                ));
            }
        }

        turn_entries.push('\n');
    }

    SESSION_HISTORY_TEMPLATE
        .replace(
            "%%CURRENT_TIME%%",
            &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .replace("%%TURN_COUNT%%", &manifests.len().to_string())
        .replace("%%TURN_ENTRIES%%", turn_entries.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_persistence_creation() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None).await;
        assert!(persistence.is_ok());
    }

    #[tokio::test]
    async fn test_iteration_tracking() {
        let temp_dir = TempDir::new().unwrap();
        let mut persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        assert_eq!(persistence.current_iteration(), 1);
        assert_eq!(persistence.start_new_iteration(), 2);
        assert_eq!(persistence.current_iteration(), 2);
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
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
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
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
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
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        let result = persistence.read_artifact("nonexistent.txt").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn test_read_artifact_path_traversal() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
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

    // ========================================================================
    // Session Namespace Tests
    // ========================================================================

    #[tokio::test]
    async fn test_session_id_creates_namespaced_directory() {
        let temp_dir = TempDir::new().unwrap();
        let session_id = "cs_test123".to_string();
        let persistence =
            ExecutionPersistence::new(temp_dir.path().join("memory"), Some(session_id.clone()))
                .await
                .unwrap();

        assert_eq!(persistence.session_id(), Some("cs_test123"));

        // Verify the run directory is under the session namespace
        let expected_prefix = temp_dir
            .path()
            .join("memory")
            .join(&session_id)
            .join(persistence.run_id());
        assert_eq!(persistence.base_path, expected_prefix);
        assert!(expected_prefix.exists());
    }

    #[tokio::test]
    async fn test_session_id_path_traversal_rejected() {
        let temp_dir = TempDir::new().unwrap();
        for bad_id in &["../escape", "foo/bar", "..\\win", ""] {
            let result =
                ExecutionPersistence::new(temp_dir.path().join("memory"), Some(bad_id.to_string()))
                    .await;
            assert!(result.is_err(), "Should reject session_id: {:?}", bad_id);
            let err = result.err().unwrap();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[tokio::test]
    async fn test_no_session_id_uses_flat_layout() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        assert!(persistence.session_id().is_none());

        // Verify flat layout: memory/{run_id}/
        let expected = temp_dir.path().join("memory").join(persistence.run_id());
        assert_eq!(persistence.base_path, expected);
    }

    // ========================================================================
    // Run Manifest Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manifest_serde_roundtrip() {
        let manifest = RunManifest {
            run_id: "test-run-id".to_string(),
            session_id: Some("cs_abc".to_string()),
            timestamp: "2026-03-19T12:00:00Z".to_string(),
            goal: "Test the system".to_string(),
            status: RunStatus::Success,
            iterations: 2,
            quality_score: Some(0.95),
            routing_mode: Some(RoutingMode::Orchestrated),
            task_summaries: vec![
                TaskSummary {
                    task_id: 0,
                    description: "First task".to_string(),
                    status: TaskStatus::Complete,
                    worker: Some("research".to_string()),
                    result_preview: Some("The answer is 42".to_string()),
                },
                TaskSummary {
                    task_id: 1,
                    description: "Second task".to_string(),
                    status: TaskStatus::Failed,
                    worker: None,
                    result_preview: None,
                },
            ],
            artifact_paths: vec!["task-0-result.txt".to_string()],
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let deserialized: RunManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.run_id, "test-run-id");
        assert_eq!(deserialized.session_id, Some("cs_abc".to_string()));
        assert_eq!(deserialized.status, RunStatus::Success);
        assert_eq!(deserialized.iterations, 2);
        assert_eq!(deserialized.quality_score, Some(0.95));
        assert_eq!(deserialized.task_summaries.len(), 2);
        assert_eq!(deserialized.task_summaries[0].status, TaskStatus::Complete);
        assert_eq!(deserialized.task_summaries[1].status, TaskStatus::Failed);
        assert_eq!(deserialized.artifact_paths, vec!["task-0-result.txt"]);
    }

    #[tokio::test]
    async fn test_write_manifest() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        let manifest = RunManifest {
            run_id: persistence.run_id().to_string(),
            session_id: None,
            timestamp: "2026-03-19T12:00:00Z".to_string(),
            goal: "Test goal".to_string(),
            status: RunStatus::PartialSuccess,
            iterations: 1,
            quality_score: Some(0.6),
            routing_mode: Some(RoutingMode::Routed),
            task_summaries: vec![],
            artifact_paths: vec![],
        };

        let path = persistence.write_manifest(&manifest).await.unwrap();
        assert!(path.exists());

        // Read back and verify
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let read_back: RunManifest = serde_json::from_str(&content).unwrap();
        assert_eq!(read_back.goal, "Test goal");
        assert_eq!(read_back.status, RunStatus::PartialSuccess);
    }

    #[tokio::test]
    async fn test_write_manifest_disabled() {
        let persistence = ExecutionPersistence::disabled();
        let manifest = RunManifest {
            run_id: String::new(),
            session_id: None,
            timestamp: String::new(),
            goal: String::new(),
            status: RunStatus::Failed,
            iterations: 0,
            quality_score: None,
            routing_mode: None,
            task_summaries: vec![],
            artifact_paths: vec![],
        };
        let path = persistence.write_manifest(&manifest).await.unwrap();
        assert_eq!(path, PathBuf::new());
    }

    #[tokio::test]
    async fn test_run_status_serde() {
        // Verify snake_case serialization
        let json = serde_json::to_string(&RunStatus::PartialSuccess).unwrap();
        assert_eq!(json, "\"partial_success\"");

        let json = serde_json::to_string(&RunStatus::Success).unwrap();
        assert_eq!(json, "\"success\"");

        let json = serde_json::to_string(&RunStatus::Failed).unwrap();
        assert_eq!(json, "\"failed\"");
    }

    // ========================================================================
    // Session History Tests
    // ========================================================================

    fn make_test_manifest(run_id: &str, timestamp: &str, goal: &str) -> RunManifest {
        RunManifest {
            run_id: run_id.to_string(),
            session_id: Some("cs_test".to_string()),
            timestamp: timestamp.to_string(),
            goal: goal.to_string(),
            status: RunStatus::Success,
            iterations: 1,
            quality_score: Some(0.95),
            routing_mode: Some(RoutingMode::Routed),
            task_summaries: vec![TaskSummary {
                task_id: 0,
                description: "Compute mean".to_string(),
                status: TaskStatus::Complete,
                worker: Some("statistics".to_string()),
                result_preview: Some("Result: 20".to_string()),
            }],
            artifact_paths: vec![],
        }
    }

    #[tokio::test]
    async fn test_load_session_manifests_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let result = load_session_manifests(temp_dir.path(), "cs_nonexistent", "exclude-me", 3)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_load_session_manifests_excludes_current_run() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir.path().join("cs_test");

        // Create two run directories with manifests
        let run1_dir = session_dir.join("run-1");
        let run2_dir = session_dir.join("run-2");
        fs::create_dir_all(&run1_dir).await.unwrap();
        fs::create_dir_all(&run2_dir).await.unwrap();

        let m1 = make_test_manifest("run-1", "2026-03-20T01:00:00Z", "First query");
        let m2 = make_test_manifest("run-2", "2026-03-20T02:00:00Z", "Second query");

        fs::write(
            run1_dir.join("manifest.json"),
            serde_json::to_string_pretty(&m1).unwrap(),
        )
        .await
        .unwrap();
        fs::write(
            run2_dir.join("manifest.json"),
            serde_json::to_string_pretty(&m2).unwrap(),
        )
        .await
        .unwrap();

        // Exclude run-2 (current run)
        let result = load_session_manifests(temp_dir.path(), "cs_test", "run-2", 3)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].run_id, "run-1");
    }

    #[tokio::test]
    async fn test_load_session_manifests_sorts_by_timestamp_desc() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir.path().join("cs_test");

        // Create runs out of chronological order
        for (id, ts) in &[
            ("run-a", "2026-03-20T03:00:00Z"),
            ("run-b", "2026-03-20T01:00:00Z"),
            ("run-c", "2026-03-20T02:00:00Z"),
        ] {
            let dir = session_dir.join(id);
            fs::create_dir_all(&dir).await.unwrap();
            let m = make_test_manifest(id, ts, &format!("Query {}", id));
            fs::write(
                dir.join("manifest.json"),
                serde_json::to_string_pretty(&m).unwrap(),
            )
            .await
            .unwrap();
        }

        let result = load_session_manifests(temp_dir.path(), "cs_test", "exclude-none", 10)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        // Most recent first
        assert_eq!(result[0].run_id, "run-a");
        assert_eq!(result[1].run_id, "run-c");
        assert_eq!(result[2].run_id, "run-b");
    }

    #[tokio::test]
    async fn test_load_session_manifests_respects_limit() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir.path().join("cs_test");

        for i in 0..5 {
            let id = format!("run-{}", i);
            let dir = session_dir.join(&id);
            fs::create_dir_all(&dir).await.unwrap();
            let m = make_test_manifest(&id, &format!("2026-03-20T0{}:00:00Z", i), "Query");
            fs::write(
                dir.join("manifest.json"),
                serde_json::to_string_pretty(&m).unwrap(),
            )
            .await
            .unwrap();
        }

        let result = load_session_manifests(temp_dir.path(), "cs_test", "exclude-none", 2)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn test_load_session_manifests_skips_latest_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir.path().join("cs_test");

        let run_dir = session_dir.join("run-1");
        fs::create_dir_all(&run_dir).await.unwrap();
        let m = make_test_manifest("run-1", "2026-03-20T01:00:00Z", "Query");
        fs::write(
            run_dir.join("manifest.json"),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .await
        .unwrap();

        // Create a "latest" symlink (should be skipped)
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("run-1", session_dir.join("latest")).unwrap();
        }

        let result = load_session_manifests(temp_dir.path(), "cs_test", "exclude-none", 10)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].run_id, "run-1");
    }

    #[test]
    fn test_build_session_context_empty() {
        let result = build_session_context(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_session_context_single_turn() {
        let manifests = vec![make_test_manifest(
            "run-1",
            "2026-03-20T01:57:24Z",
            "Compute mean of [10,20,30]",
        )];

        let result = build_session_context(&manifests);

        assert!(result.contains("## Session History"));
        assert!(result.contains("Current time: "));
        assert!(result.contains("1 previous orchestration run(s)"));
        assert!(result.contains("### Turn 1 (2026-03-20T01:57:24Z)"));
        assert!(result.contains("Success"));
        assert!(result.contains("quality: 0.95"));
        assert!(result.contains("Compute mean of [10,20,30]"));
        assert!(result.contains("Task 0 [statistics]: Compute mean"));
        assert!(result.contains("Result: 20"));
        // Guidance text from template
        assert!(result.contains("Avoid redundant work"));
        assert!(result.contains("Embed concrete values for workers"));
    }

    #[test]
    fn test_build_session_context_multi_turn_chronological_order() {
        // Manifests arrive most-recent-first from load_session_manifests
        let manifests = vec![
            make_test_manifest("run-2", "2026-03-20T02:00:00Z", "Second query"),
            make_test_manifest("run-1", "2026-03-20T01:00:00Z", "First query"),
        ];

        let result = build_session_context(&manifests);

        assert!(result.contains("2 previous orchestration run(s)"));
        // Turn 1 should be the older one (chronological order)
        let turn1_pos = result.find("### Turn 1").unwrap();
        let turn2_pos = result.find("### Turn 2").unwrap();
        assert!(turn1_pos < turn2_pos);
        assert!(result[turn1_pos..turn2_pos].contains("First query"));
        assert!(result[turn2_pos..].contains("Second query"));
    }

    #[test]
    fn test_build_session_context_failed_task() {
        let manifest = RunManifest {
            run_id: "run-fail".to_string(),
            session_id: Some("cs_test".to_string()),
            timestamp: "2026-03-20T01:00:00Z".to_string(),
            goal: "Failing query".to_string(),
            status: RunStatus::Failed,
            iterations: 1,
            quality_score: Some(0.3),
            routing_mode: Some(RoutingMode::Orchestrated),
            task_summaries: vec![TaskSummary {
                task_id: 0,
                description: "Bad task".to_string(),
                status: TaskStatus::Failed,
                worker: Some("worker1".to_string()),
                result_preview: Some("Connection refused".to_string()),
            }],
            artifact_paths: vec![],
        };

        let result = build_session_context(&[manifest]);

        assert!(result.contains("Failed"));
        assert!(result.contains("FAILED: \"Connection refused\""));
    }
}
