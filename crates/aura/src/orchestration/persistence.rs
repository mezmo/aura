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
//!     │   └── task-0-default-iter-1-result.txt
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
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::fs;
use tokio::sync::Notify;

use super::events::RoutingMode;
use super::park::{PARKED_DOCUMENT_SUFFIX, RESUMING_DOCUMENT_SUFFIX};
use super::types::{Plan, TaskStatus};

// ============================================================================
// Filename Helpers
// ============================================================================

/// Sanitize a string for use as a filename component.
///
/// Lowercases, replaces non-alphanumeric characters with `-`, collapses
/// consecutive `-`, and trims leading/trailing `-`. Returns `"unknown"` for
/// empty input. Used for worker names and tool names in artifact filenames.
pub fn sanitize_filename_component(s: &str) -> String {
    let s = s.to_lowercase();
    let sanitized: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = sanitized
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "unknown".to_string()
    } else {
        collapsed
    }
}

/// True when `s` is safe to use as a single path component: non-empty, no path
/// separators, no parent references. Artifact filenames and run IDs come from
/// untrusted tool/LLM input and are validated with this before being joined
/// into a persistence path.
pub(crate) fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\\') && !s.contains("..")
}

/// Whether `run_id` has a parked checkpoint document under `parked_dir`,
/// under either the published or the resuming filename.
async fn run_has_parked_document(parked_dir: &Path, run_id: &str) -> bool {
    for suffix in [PARKED_DOCUMENT_SUFFIX, RESUMING_DOCUMENT_SUFFIX] {
        if parked_dir
            .join(format!("{run_id}{suffix}"))
            .try_exists()
            .is_ok_and(|e| e)
        {
            return true;
        }
    }
    false
}

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
    /// Number of plan-execute cycles.
    pub iterations: usize,
    /// How the coordinator routed this query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_mode: Option<RoutingMode>,
    /// Human-readable outcome description (e.g. "Answered directly", "3/4 tasks completed").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Coordinator's summary of the final response (from respond_directly or synthesis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_summary: Option<String>,
    /// Summary of each task in the plan.
    pub task_summaries: Vec<TaskSummary>,
    /// Relative paths to large artifact files.
    pub artifact_paths: Vec<String>,
    /// Phase-level wall-clock timings for the final iteration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_timings: Option<super::types::IterationTimings>,
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
    /// Task result preview for session history. Worker-provided summary from
    /// `submit_result` when available; falls back to first ~200 chars of result.
    pub result_preview: Option<String>,
    /// Worker-reported confidence from `submit_result` (high/medium/low).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Structured failure classification (if failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<super::types::FailureCategory>,
    /// Error message from TaskState::Failed (if failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Structured failure detail for session history rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_context: Option<ErrorContext>,
    /// Condensed tool call chain for this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_trace: Vec<ToolTraceEntry>,
    /// Artifacts produced by this task (hierarchical view of flat storage).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactEntry>,
}

/// Structured failure detail for a task in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorContext {
    pub category: super::types::FailureCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_call: Option<String>,
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_result: Option<String>,
}

/// Condensed tool call entry for the manifest tool trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceEntry {
    pub tool: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    pub duration_ms: u64,
    pub outcome: ToolOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_filename: Option<String>,
}

/// Outcome of a single tool call in the trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Success { output_bytes: u64 },
    Error { message: String },
}

/// An artifact file produced during a task's execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub filename: String,
    pub size_bytes: u64,
    pub kind: ArtifactKind,
}

/// Distinguishes worker result artifacts from promoted tool output artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Result,
    ToolOutput { tool_name: String },
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
    /// Run stopped at the park verdict awaiting human decisions; the parked
    /// checkpoint document is the record.
    Parked,
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
    /// Final result
    pub result: Option<String>,
    /// Worker-provided summary from `submit_result` tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
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
///
/// Tracks in-flight async writes via `in_flight` / `drain_notify` so callers
/// can wait for all fire-and-forget `on_complete` persistence hooks to finish
/// before reading back artifacts. The `Arc` counter and notify fields live
/// outside the Mutex so increment/decrement is lock-free, but `on_complete`
/// still acquires the Mutex for the actual file I/O — callers must release
/// the Mutex before calling `drain()` to avoid deadlock.
#[derive(Clone)]
pub struct ExecutionPersistence {
    base_path: PathBuf,
    run_id: String,
    session_id: Option<String>,
    current_iteration: usize,
    enabled: bool,
    in_flight: Arc<AtomicUsize>,
    drain_notify: Arc<Notify>,
    /// Condensed tool-call trace per task id.
    tool_traces: Arc<StdMutex<HashMap<usize, Vec<ToolTraceEntry>>>>,
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
        let run_id = uuid::Uuid::now_v7().to_string();
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
            in_flight: Arc::new(AtomicUsize::new(0)),
            drain_notify: Arc::new(Notify::new()),
            tool_traces: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    /// Rebind persistence to a run directory recorded by an earlier
    /// execution, binding a later resume to that run's artifacts.
    ///
    /// Uses the recorded `run_id` (not a fresh one), validates it against
    /// path traversal, and re-creates the directory if it has been removed.
    /// Iterations restart at 1; the resume's iterations write alongside the
    /// original ones.
    pub async fn reopen<P: AsRef<Path>>(
        base_path: P,
        session_id: Option<String>,
        run_id: &str,
    ) -> io::Result<Self> {
        if let Some(ref sid) = session_id
            && (sid.is_empty() || sid.contains('/') || sid.contains('\\') || sid.contains(".."))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid session_id for persistence path: {:?}", sid),
            ));
        }
        if !is_safe_path_component(run_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid run_id for persistence reopen: {:?}", run_id),
            ));
        }

        let effective_base = match session_id.as_deref() {
            Some(sid) => base_path.as_ref().join(sid),
            None => base_path.as_ref().to_path_buf(),
        };
        let run_path = effective_base.join(run_id);
        fs::create_dir_all(&run_path).await?;

        tracing::info!("🗂️ Execution persistence reopened: {}", run_path.display());

        Ok(Self {
            base_path: run_path,
            run_id: run_id.to_string(),
            session_id,
            current_iteration: 1,
            enabled: true,
            in_flight: Arc::new(AtomicUsize::new(0)),
            drain_notify: Arc::new(Notify::new()),
            tool_traces: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    /// Prune oldest run directories if the session exceeds `max_runs`.
    ///
    /// Skips the current run, the `latest` symlink, the `parked` checkpoint
    /// directory, and any run holding a parked document (either filename) —
    /// a parked run's artifacts must survive pruning for its resume to read.
    /// Directories are sorted lexicographically (UUID v7 = chronological
    /// order) and the oldest are removed first. Best-effort: errors on
    /// individual deletions are logged but don't fail the operation.
    pub async fn prune_session_runs(&self, max_runs: usize) {
        if !self.enabled || max_runs == 0 || self.session_id.is_none() {
            return;
        }

        let session_dir = match self.base_path.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        let parked_dir = session_dir.join("parked");

        let mut run_dirs: Vec<String> = Vec::new();
        let mut entries = match fs::read_dir(&session_dir).await {
            Ok(e) => e,
            Err(_) => return,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            match entry.file_type().await {
                Ok(ft) if ft.is_dir() => {}
                _ => continue,
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == "latest" || name == "parked" || name == self.run_id {
                    continue;
                }
                if run_has_parked_document(&parked_dir, name).await {
                    tracing::info!("Skipping prune of run {} with a parked document", name);
                    continue;
                }
                run_dirs.push(name.to_string());
            }
        }

        if run_dirs.len() < max_runs {
            return;
        }

        run_dirs.sort();
        let to_remove = run_dirs.len() - max_runs + 1;
        for dir_name in run_dirs.iter().take(to_remove) {
            let path = session_dir.join(dir_name);
            match fs::remove_dir_all(&path).await {
                Ok(()) => tracing::info!("Pruned old run directory: {}", dir_name),
                Err(e) => tracing::warn!("Failed to prune run directory {}: {}", dir_name, e),
            }
        }
    }

    /// Create a disabled persistence manager (no-op writes).
    pub fn disabled() -> Self {
        Self {
            base_path: PathBuf::new(),
            run_id: uuid::Uuid::new_v4().to_string(),
            session_id: None,
            current_iteration: 1,
            enabled: false,
            in_flight: Arc::new(AtomicUsize::new(0)),
            drain_notify: Arc::new(Notify::new()),
            tool_traces: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Whether persistence is enabled (writes go to disk).
    pub fn is_enabled(&self) -> bool {
        self.enabled
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

    /// Arc handle to the in-flight write counter.
    ///
    /// Shared with `PersistenceWrapper` instances so fire-and-forget
    /// `on_complete` hooks can increment/decrement without holding the Mutex.
    pub fn in_flight_counter(&self) -> Arc<AtomicUsize> {
        self.in_flight.clone()
    }

    /// Arc handle to the drain notification channel.
    pub fn drain_notify(&self) -> Arc<Notify> {
        self.drain_notify.clone()
    }

    /// Wait for all in-flight persistence writes to complete, bounded by `timeout`.
    ///
    /// Returns `true` if the counter reached zero before the deadline.
    pub async fn drain(&self, timeout: Duration) -> bool {
        // Yield to let recently-spawned on_complete tasks poll their first
        // increment before we check the counter (closes TOCTOU window between
        // tokio::spawn and fetch_add inside on_complete).
        tokio::task::yield_now().await;

        if self.in_flight.load(Ordering::Acquire) == 0 {
            return true;
        }
        tokio::select! {
            _ = async {
                while self.in_flight.load(Ordering::Acquire) > 0 {
                    self.drain_notify.notified().await;
                }
            } => true,
            _ = tokio::time::sleep(timeout) => {
                let remaining = self.in_flight.load(Ordering::Acquire);
                tracing::warn!(remaining, "Persistence drain timed out");
                false
            }
        }
    }

    /// Get iteration directory path (flat, directly under run dir).
    pub(super) fn iteration_path(&self) -> PathBuf {
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
    ///
    /// `phase_index` distinguishes multiple coordinator calls within one
    /// iteration (e.g. `0` for initial planning, `1` for the post-execution
    /// continuation decision) so successive calls don't overwrite one another.
    pub async fn write_planning_phase(
        &self,
        phase_index: usize,
        prompt: &str,
        response: &str,
    ) -> io::Result<PathBuf> {
        if !self.enabled {
            return Ok(PathBuf::new());
        }

        let iter_path = self.iteration_path();
        fs::create_dir_all(&iter_path).await?;

        fs::write(
            iter_path.join(format!("planning.{phase_index}.prompt.txt")),
            prompt,
        )
        .await?;
        fs::write(
            iter_path.join(format!("planning.{phase_index}.response.txt")),
            response,
        )
        .await?;

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

        // Write prompt and response with namespaced filenames.
        // Tool calls are tracked in memory via `record_tool_trace()`;
        // nothing to write for them here.
        let prompt_file = self.task_attempt_filename(task_id, attempt, "prompt.txt");
        let response_file = self.task_attempt_filename(task_id, attempt, "response.txt");
        fs::write(iter_path.join(&prompt_file), prompt).await?;
        fs::write(iter_path.join(&response_file), response).await?;

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

    /// Get relative path for logging.
    pub fn relative_path(&self, task_id: usize, attempt: usize) -> String {
        self.task_attempt_filename(task_id, attempt, "*")
    }

    // ========================================================================
    // Result Artifact Methods
    // ========================================================================

    /// Directory for result artifacts (run-level, not per-iteration).
    pub fn artifacts_path(&self) -> PathBuf {
        self.base_path.join("artifacts")
    }

    /// Write a large result to an artifact file.
    ///
    /// Returns the artifact filename (not the full path) for reference in summaries.
    /// Filenames are iteration-namespaced to avoid collisions across replans:
    /// `task-{id}-{worker}-iter-{n}-result.txt`
    pub async fn write_result_artifact(
        &self,
        task_id: usize,
        worker_name: Option<&str>,
        iteration: usize,
        result: &str,
    ) -> io::Result<String> {
        if !self.enabled {
            return Ok(String::new());
        }

        let artifacts_dir = self.artifacts_path();
        fs::create_dir_all(&artifacts_dir).await?;

        let worker = sanitize_filename_component(worker_name.unwrap_or("default"));
        let filename = format!("task-{}-{}-iter-{}-result.txt", task_id, worker, iteration);
        let artifact_path = artifacts_dir.join(&filename);
        fs::write(&artifact_path, result).await?;

        tracing::info!(
            "Written result artifact ({} chars) to: {}",
            result.len(),
            artifact_path.display()
        );
        Ok(filename)
    }

    /// Write a tool output to an artifact file.
    ///
    /// Returns the artifact filename for reference in footers and tool traces.
    /// Filename: `task-{id}-{worker}-iter-{n}-{tool_name}-{call_idx}-output.txt`
    pub async fn write_tool_output_artifact(
        &self,
        task_id: usize,
        worker_name: &str,
        iteration: usize,
        tool_name: &str,
        call_idx: usize,
        output: &str,
    ) -> io::Result<String> {
        if !self.enabled {
            return Ok(String::new());
        }

        let artifacts_dir = self.artifacts_path();
        fs::create_dir_all(&artifacts_dir).await?;

        let worker = sanitize_filename_component(worker_name);
        let tool = sanitize_filename_component(tool_name);
        let filename = format!(
            "task-{}-{}-iter-{}-{}-{}-output.txt",
            task_id, worker, iteration, tool, call_idx
        );
        let artifact_path = artifacts_dir.join(&filename);
        fs::write(&artifact_path, output).await?;

        tracing::info!(
            "Written tool output artifact ({} chars) to: {}",
            output.len(),
            artifact_path.display()
        );
        Ok(filename)
    }

    /// Read an artifact file by filename.
    ///
    /// Resolves the path via [`artifact_path`](Self::artifact_path) (which
    /// validates against path traversal), then reads it.
    pub async fn read_artifact(&self, filename: &str) -> io::Result<String> {
        if !self.enabled {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Persistence is disabled",
            ));
        }
        let artifact_path = self.artifact_path(filename)?;
        fs::read_to_string(&artifact_path).await
    }

    /// Read an artifact from a different run in the same session.
    ///
    /// Resolves the path via
    /// [`artifact_path_cross_run`](Self::artifact_path_cross_run), then adds a
    /// canonicalized containment check (the resolved path must stay under the
    /// session directory) before reading.
    pub async fn read_artifact_cross_run(
        &self,
        filename: &str,
        run_id: &str,
    ) -> io::Result<String> {
        if !self.enabled {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Persistence is disabled",
            ));
        }

        let artifact_path = self.artifact_path_cross_run(filename, run_id)?;

        // Defense-in-depth: the resolved path must canonicalize to somewhere
        // under the session directory (catches symlink escapes the lexical
        // component checks in artifact_path_cross_run can't see).
        let session_dir = self
            .base_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No parent directory"))?;
        let canonical_session = match fs::canonicalize(session_dir).await {
            Ok(p) => p,
            Err(_) => session_dir.to_path_buf(),
        };
        let canonical_artifact = fs::canonicalize(&artifact_path)
            .await
            .map_err(|e| io::Error::new(e.kind(), format!("Artifact not found: {e}")))?;
        if !canonical_artifact.starts_with(&canonical_session) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cross-run artifact path escapes session directory",
            ));
        }

        fs::read_to_string(&artifact_path).await
    }

    /// Resolve the absolute path of a current-run artifact.
    ///
    /// Validates `filename` against path traversal but does **not** check that
    /// the file exists or read it. Used both by [`read_artifact`](Self::read_artifact)
    /// and to build an in-place `file=` reference for the scratchpad read tools
    /// when an artifact is too large to inline.
    pub fn artifact_path(&self, filename: &str) -> io::Result<PathBuf> {
        if !is_safe_path_component(filename) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid artifact filename",
            ));
        }
        Ok(self.artifacts_path().join(filename))
    }

    /// Resolve the absolute path of a cross-run artifact within this session.
    ///
    /// Resolves against `{session_dir}/{run_id}/artifacts/{filename}` where
    /// `session_dir` is the parent of the current run directory. Validates both
    /// components against path traversal but does **not** check existence; the
    /// caller (e.g. [`read_artifact_cross_run`](Self::read_artifact_cross_run))
    /// performs the canonicalized containment check when it reads.
    pub fn artifact_path_cross_run(&self, filename: &str, run_id: &str) -> io::Result<PathBuf> {
        if !is_safe_path_component(run_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid run_id for cross-run artifact read",
            ));
        }
        if !is_safe_path_component(filename) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid artifact filename",
            ));
        }
        let session_dir = self
            .base_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No parent directory"))?;
        Ok(session_dir.join(run_id).join("artifacts").join(filename))
    }

    /// List all artifact filenames.
    pub async fn list_artifacts(&self) -> io::Result<Vec<String>> {
        if !self.enabled {
            return Ok(Vec::new());
        }

        let artifacts_dir = self.artifacts_path();
        let mut entries = match fs::read_dir(&artifacts_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut filenames = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                filenames.push(name.to_string());
            }
        }
        filenames.sort();
        Ok(filenames)
    }

    /// List all artifact filenames with file sizes.
    pub async fn list_artifacts_with_metadata(&self) -> io::Result<Vec<(String, u64)>> {
        if !self.enabled {
            return Ok(Vec::new());
        }

        let artifacts_dir = self.artifacts_path();
        let mut entries = match fs::read_dir(&artifacts_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut results = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                results.push((name.to_string(), size));
            }
        }
        results.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(results)
    }

    /// All tool traces recorded for a task so far, in call-completion order
    /// across every iteration and attempt.
    pub fn tool_traces_for_task(&self, task_id: usize) -> Vec<ToolTraceEntry> {
        if !self.enabled {
            return Vec::new();
        }
        self.tool_traces
            .lock()
            .unwrap()
            .get(&task_id)
            .cloned()
            .unwrap_or_default()
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

    /// Record a condensed tool trace entry for a task.
    ///
    /// Called by PersistenceWrapper as each tool call completes. Traces are
    /// held in memory for continuation-prompt rendering and reach disk only
    /// via the run manifest (`TaskSummary.tool_trace`); full tool outputs are
    /// captured by artifact promotion and OTel, not here.
    pub fn record_tool_trace(&self, task_id: usize, entry: ToolTraceEntry) {
        if !self.enabled {
            return;
        }
        self.tool_traces
            .lock()
            .unwrap()
            .entry(task_id)
            .or_default()
            .push(entry);
    }
}

// ============================================================================
// Session History — Cross-Run Manifest Loading
// ============================================================================

/// Session history template loaded at compile time.
const SESSION_HISTORY_TEMPLATE: &str = include_str!("../prompts/session_history.md");

/// Load run manifests from prior runs in a session directory.
///
/// Reads `{base_path}/{session_id}/*/manifest.json`, excludes the current
/// run, selects the `limit` most recently *created* runs (UUIDv7 dir-name
/// order), and returns their manifests sorted by recorded timestamp
/// descending. Creation order and completion-timestamp order diverge only
/// for overlapping runs of one session. A session containing any
/// non-canonical-v7 run dir (e.g. a UUIDv4 name) falls back to
/// reading every manifest and selecting by completion timestamp.
pub async fn load_session_manifests(
    base_path: &Path,
    session_id: &str,
    exclude_run_id: &str,
    limit: usize,
) -> io::Result<Vec<RunManifest>> {
    let session_dir = base_path.join(session_id);
    let mut entries = match fs::read_dir(&session_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    // Collect candidate run dirs first, newest first, so manifest reads can
    // stop at `limit` instead of reading every prior run. Run IDs are UUIDv7,
    // so lexicographic order is creation order (the same
    // invariant prune_session_runs relies on).
    let mut run_dirs: Vec<String> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        match entry.file_type().await {
            Ok(ft) if ft.is_dir() => {}
            _ => continue,
        }
        if let Ok(dir_name) = entry.file_name().into_string()
            && dir_name != exclude_run_id
            && dir_name != "latest"
        {
            run_dirs.push(dir_name);
        }
    }
    run_dirs.sort_unstable_by(|a, b| b.cmp(a));

    // UUIDv4 run dir names carry no chronology — a v4 starting with 'f'
    // would outrank every
    // v7 starting with '0' and starve recent runs out of the limit. If any
    // candidate is not a canonical (lowercase hyphenated) v7 name — parse
    // alone also accepts braced/URN/uppercase forms whose string order is
    // not UUID order — read every manifest and let the timestamp sort pick,
    // matching the run's actual recency.
    let all_v7 = run_dirs.iter().all(|name| {
        uuid::Uuid::parse_str(name)
            .is_ok_and(|u| u.get_version_num() == 7 && u.hyphenated().to_string() == *name)
    });
    let read_cap = if all_v7 { limit } else { usize::MAX };

    let mut manifests = Vec::new();
    for dir_name in run_dirs {
        if manifests.len() >= read_cap {
            break;
        }
        let manifest_path = session_dir.join(&dir_name).join("manifest.json");
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
            // A run dir without a manifest is expected (crashed or in-flight
            // run) and skipped without noise.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    "Failed to read manifest at {}: {}",
                    manifest_path.display(),
                    e
                );
            }
        }
    }

    // Manifests were read in dir-name (creation) order; sort by recorded
    // timestamp so the documented contract holds even if the two disagree.
    manifests.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
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

        turn_entries.push_str(&format!(
            "### Turn {} ({}) — {}\n",
            turn_num, manifest.timestamp, status
        ));
        turn_entries.push_str(&format!("Goal: \"{}\"\n", manifest.goal));

        if let Some(outcome) = &manifest.outcome {
            turn_entries.push_str(&format!("Outcome: {}\n", outcome));
        }

        if let Some(summary) = &manifest.response_summary {
            turn_entries.push_str(&format!("Response: \"{}\"\n", summary));
        }

        if !manifest.task_summaries.is_empty() {
            turn_entries.push_str("Tasks:\n");
            for task in &manifest.task_summaries {
                render_task_summary(task, &mut turn_entries);
            }
        }

        let has_artifacts = manifest
            .task_summaries
            .iter()
            .any(|t| !t.artifacts.is_empty());
        if has_artifacts {
            turn_entries.push_str(&format!(
                "  (use run_id=\"{}\" with read_artifact for cross-run access)\n",
                manifest.run_id
            ));
        }

        turn_entries.push('\n');
    }

    SESSION_HISTORY_TEMPLATE
        .replace("%%TURN_COUNT%%", &manifests.len().to_string())
        .replace("%%TURN_ENTRIES%%", turn_entries.trim_end())
}

fn render_task_summary(task: &TaskSummary, out: &mut String) {
    let worker = task.worker.as_deref().unwrap_or("unassigned");

    match task.status {
        TaskStatus::Complete => {
            let confidence_tag = task
                .confidence
                .as_deref()
                .map(|c| format!(" ({})", c))
                .unwrap_or_default();
            out.push_str(&format!(
                "  Task {} [{}] — Complete{}\n",
                task.task_id, worker, confidence_tag
            ));
            out.push_str(&format!("    \"{}\"\n", task.description));
            if let Some(preview) = &task.result_preview {
                out.push_str(&format!("    Summary: \"{}\"\n", preview));
            }
        }
        TaskStatus::Failed => {
            let category_tag = task
                .failure_category
                .as_ref()
                .map(|c| format!(" ({})", c))
                .unwrap_or_default();
            out.push_str(&format!(
                "  Task {} [{}] — FAILED{}\n",
                task.task_id, worker, category_tag
            ));
            out.push_str(&format!("    \"{}\"\n", task.description));
            if let Some(error) = &task.error {
                out.push_str(&format!("    Error: {}\n", error));
            }
            if let Some(ctx) = &task.error_context {
                if let Some(tool) = &ctx.last_tool_call {
                    out.push_str(&format!("    Last tool: {}\n", tool));
                }
                if let Some(partial) = &ctx.partial_result {
                    out.push_str(&format!("    Partial progress: {}\n", partial));
                }
            }
        }
        _ => {
            out.push_str(&format!(
                "  Task {} [{}] — {}\n",
                task.task_id, worker, task.status
            ));
            out.push_str(&format!("    \"{}\"\n", task.description));
        }
    }

    if !task.tool_trace.is_empty() {
        let chain: Vec<String> = task
            .tool_trace
            .iter()
            .map(|t| {
                let duration = format!("{:.1}s", t.duration_ms as f64 / 1000.0);
                match &t.outcome {
                    ToolOutcome::Success { .. } => format!("{} ({})", t.tool, duration),
                    ToolOutcome::Error { message } => {
                        format!("{} (FAILED: {})", t.tool, message)
                    }
                }
            })
            .collect();
        out.push_str(&format!("    Tool chain: {}\n", chain.join(" → ")));
    }

    if !task.artifacts.is_empty() {
        let listing: Vec<String> = task
            .artifacts
            .iter()
            .map(|a| format!("{} ({}B)", a.filename, a.size_bytes))
            .collect();
        out.push_str(&format!("    Artifacts: {}\n", listing.join(", ")));
    }
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

    /// Layout invariant that `read_artifact`'s in-place pointer relies on: the
    /// artifacts dir and every iteration's scratchpad dir live under the run
    /// dir, and the run dir lives under the read root (the session dir, i.e.
    /// the run dir's parent). If this drifts, a worker's scratchpad read_root
    /// would no longer cover the artifacts dir and oversized artifacts would be
    /// refused instead of explorable in place. Covers both layouts.
    #[tokio::test]
    async fn test_artifacts_under_read_root_invariant() {
        for session in [None, Some("session-1".to_string())] {
            let temp_dir = TempDir::new().unwrap();
            let mut persistence =
                ExecutionPersistence::new(temp_dir.path().join("memory"), session.clone())
                    .await
                    .unwrap();

            let run_dir = persistence.run_path().to_path_buf();
            // Read root used by worker scratchpad storage (orchestrator.rs).
            let read_root = run_dir
                .parent()
                .expect("run dir has a parent")
                .to_path_buf();

            // Artifacts dir is under the run dir → under the read root.
            assert!(
                persistence.artifacts_path().starts_with(&run_dir),
                "artifacts dir {} must be under run dir {}",
                persistence.artifacts_path().display(),
                run_dir.display(),
            );
            assert!(
                persistence.artifacts_path().starts_with(&read_root),
                "artifacts dir {} must be under read root {}",
                persistence.artifacts_path().display(),
                read_root.display(),
            );

            // Every iteration's scratchpad parent is under the run dir too, so a
            // scratchpad rooted there can reach the artifacts via `../`.
            for _ in 0..3 {
                assert!(
                    persistence.iteration_path().starts_with(&run_dir),
                    "iteration dir {} must be under run dir {}",
                    persistence.iteration_path().display(),
                    run_dir.display(),
                );
                persistence.start_new_iteration();
            }
        }
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
    async fn test_write_planning_phase_indexed_files() {
        let temp_dir = TempDir::new().unwrap();
        let mut persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        // Two coordinator phases in the same iteration must not overwrite
        // one another.
        persistence
            .write_planning_phase(0, "initial-prompt", "initial-response")
            .await
            .unwrap();
        persistence
            .write_planning_phase(1, "continuation-prompt", "continuation-response")
            .await
            .unwrap();

        let iter1 = persistence.iteration_path();
        assert_eq!(
            tokio::fs::read_to_string(iter1.join("planning.0.prompt.txt"))
                .await
                .unwrap(),
            "initial-prompt",
        );
        assert_eq!(
            tokio::fs::read_to_string(iter1.join("planning.0.response.txt"))
                .await
                .unwrap(),
            "initial-response",
        );
        assert_eq!(
            tokio::fs::read_to_string(iter1.join("planning.1.prompt.txt"))
                .await
                .unwrap(),
            "continuation-prompt",
        );
        assert_eq!(
            tokio::fs::read_to_string(iter1.join("planning.1.response.txt"))
                .await
                .unwrap(),
            "continuation-response",
        );

        // The legacy unindexed filenames must not appear.
        assert!(!iter1.join("planning.prompt.txt").exists());
        assert!(!iter1.join("planning.response.txt").exists());

        // A new iteration gets its own directory, so `phase_index = 0` starts
        // fresh without colliding with the previous iteration's files.
        persistence.start_new_iteration();
        persistence
            .write_planning_phase(0, "iter2-prompt", "iter2-response")
            .await
            .unwrap();

        let iter2 = persistence.iteration_path();
        assert_ne!(iter1, iter2);
        assert_eq!(
            tokio::fs::read_to_string(iter2.join("planning.0.prompt.txt"))
                .await
                .unwrap(),
            "iter2-prompt",
        );
        // Previous iteration's artifacts remain intact.
        assert_eq!(
            tokio::fs::read_to_string(iter1.join("planning.0.prompt.txt"))
                .await
                .unwrap(),
            "initial-prompt",
        );
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
            .write_result_artifact(0, Some("research"), 1, "full result content")
            .await
            .unwrap();
        assert_eq!(filename, "task-0-research-iter-1-result.txt");

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
            .write_result_artifact(0, None, 1, "result 0")
            .await
            .unwrap();
        persistence
            .write_result_artifact(1, Some("stats"), 1, "result 1")
            .await
            .unwrap();

        let artifacts = persistence.list_artifacts().await.unwrap();
        assert_eq!(artifacts.len(), 2);
        assert!(artifacts.contains(&"task-0-default-iter-1-result.txt".to_string()));
        assert!(artifacts.contains(&"task-1-stats-iter-1-result.txt".to_string()));
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
    async fn test_read_artifact_cross_run_basic() {
        let temp_dir = TempDir::new().unwrap();
        let memory_dir = temp_dir.path().join("memory");
        let session_id = "session_xrun".to_string();

        // Create run A and write an artifact
        let run_a = ExecutionPersistence::new(&memory_dir, Some(session_id.clone()))
            .await
            .unwrap();
        let run_a_id = run_a.run_id().to_string();
        run_a
            .write_result_artifact(0, Some("sre"), 1, "prior run content")
            .await
            .unwrap();

        // Create run B
        let run_b = ExecutionPersistence::new(&memory_dir, Some(session_id))
            .await
            .unwrap();

        // Read run A's artifact from run B
        let content = run_b
            .read_artifact_cross_run("task-0-sre-iter-1-result.txt", &run_a_id)
            .await
            .unwrap();
        assert_eq!(content, "prior run content");
    }

    #[tokio::test]
    async fn test_read_artifact_cross_run_invalid_run_id() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        for bad_id in &["../escape", "foo/bar", "..\\win", ""] {
            let result = persistence
                .read_artifact_cross_run("task-0-default-iter-1-result.txt", bad_id)
                .await;
            assert!(result.is_err(), "Should reject run_id: {:?}", bad_id);
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[tokio::test]
    async fn test_disabled_persistence_artifacts() {
        let persistence = ExecutionPersistence::disabled();

        // Write returns empty string
        let filename = persistence
            .write_result_artifact(0, None, 1, "content")
            .await
            .unwrap();
        assert!(filename.is_empty());

        // Read fails
        let result = persistence
            .read_artifact("task-0-default-iter-1-result.txt")
            .await;
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
            routing_mode: Some(RoutingMode::Orchestrated),
            outcome: None,
            response_summary: None,
            task_summaries: vec![
                TaskSummary {
                    task_id: 0,
                    description: "First task".to_string(),
                    status: TaskStatus::Complete,
                    worker: Some("research".to_string()),
                    result_preview: Some("The answer is 42".to_string()),
                    confidence: None,
                    failure_category: None,
                    error: None,
                    error_context: None,
                    tool_trace: vec![],
                    artifacts: vec![],
                },
                TaskSummary {
                    task_id: 1,
                    description: "Second task".to_string(),
                    status: TaskStatus::Failed,
                    worker: None,
                    result_preview: None,
                    confidence: None,
                    failure_category: Some(super::super::types::FailureCategory::AgentError),
                    error: Some("Connection refused".to_string()),
                    error_context: None,
                    tool_trace: vec![],
                    artifacts: vec![],
                },
            ],
            artifact_paths: vec!["task-0-research-iter-1-result.txt".to_string()],
            phase_timings: None,
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let deserialized: RunManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.run_id, "test-run-id");
        assert_eq!(deserialized.session_id, Some("cs_abc".to_string()));
        assert_eq!(deserialized.status, RunStatus::Success);
        assert_eq!(deserialized.iterations, 2);
        assert_eq!(deserialized.task_summaries.len(), 2);
        assert_eq!(deserialized.task_summaries[0].status, TaskStatus::Complete);
        assert_eq!(deserialized.task_summaries[1].status, TaskStatus::Failed);
        assert_eq!(
            deserialized.artifact_paths,
            vec!["task-0-research-iter-1-result.txt"]
        );
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
            routing_mode: Some(RoutingMode::Routed),
            outcome: None,
            response_summary: None,
            task_summaries: vec![],
            artifact_paths: vec![],
            phase_timings: None,
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
            routing_mode: None,
            outcome: None,
            response_summary: None,
            task_summaries: vec![],
            artifact_paths: vec![],
            phase_timings: None,
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
            routing_mode: Some(RoutingMode::Routed),
            outcome: None,
            response_summary: None,
            task_summaries: vec![TaskSummary {
                task_id: 0,
                description: "Compute mean".to_string(),
                status: TaskStatus::Complete,
                worker: Some("statistics".to_string()),
                result_preview: Some("Result: 20".to_string()),
                confidence: None,
                failure_category: None,
                error: None,
                error_context: None,
                tool_trace: vec![],
                artifacts: vec![],
            }],
            artifact_paths: vec![],
            phase_timings: None,
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

        // Non-UUID names fall back to full traversal and timestamp sorting.
        for i in 0..5 {
            let id = format!("run-{}", i);
            let dir = session_dir.join(&id);
            fs::create_dir_all(&dir).await.unwrap();
            let m = make_test_manifest(&id, &format!("2026-03-20T0{}:00:00Z", 5 - i), "Query");
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
        assert_eq!(result[0].run_id, "run-0");
        assert_eq!(result[1].run_id, "run-1");
    }

    #[tokio::test]
    async fn test_load_session_manifests_v7_bounded_by_creation_order() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir.path().join("cs_test");

        // All-v7 session: selection stops at `limit` based on directory names.
        let ids: Vec<String> = (0..4)
            .map(|i| {
                uuid::Uuid::new_v7(uuid::Timestamp::from_unix(
                    uuid::NoContext,
                    1_750_000_000 + i,
                    0,
                ))
                .to_string()
            })
            .collect();
        for (i, id) in ids.iter().enumerate() {
            let dir = session_dir.join(id);
            fs::create_dir_all(&dir).await.unwrap();
            let m = make_test_manifest(id, &format!("2026-03-20T0{}:00:00Z", 5 - i), "Query");
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
        // Output is timestamp-sorted.
        assert_eq!(result[0].run_id, ids[2]);
        assert_eq!(result[1].run_id, ids[3]);
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
        assert!(result.contains("1 prior run(s) shown above"));
        assert!(result.contains("### Turn 1 (2026-03-20T01:57:24Z)"));
        assert!(result.contains("Success"));
        assert!(result.contains("Compute mean of [10,20,30]"));
        assert!(result.contains("Task 0 [statistics] — Complete"));
        assert!(result.contains("Summary: \"Result: 20\""));
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

        assert!(result.contains("2 prior run(s) shown above"));
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
            routing_mode: Some(RoutingMode::Orchestrated),
            outcome: None,
            response_summary: None,
            task_summaries: vec![TaskSummary {
                task_id: 0,
                description: "Bad task".to_string(),
                status: TaskStatus::Failed,
                worker: Some("worker1".to_string()),
                result_preview: Some("Connection refused".to_string()),
                confidence: None,
                failure_category: Some(super::super::types::FailureCategory::AgentError),
                error: Some("Connection refused".to_string()),
                error_context: None,
                tool_trace: vec![],
                artifacts: vec![],
            }],
            artifact_paths: vec![],
            phase_timings: None,
        };

        let result = build_session_context(&[manifest]);

        assert!(result.contains("Failed"));
        assert!(result.contains("FAILED (agent_error)"));
        assert!(result.contains("Error: Connection refused"));
    }

    #[test]
    fn test_build_session_context_with_tool_trace() {
        let manifest = RunManifest {
            run_id: "run-trace".to_string(),
            session_id: Some("cs_test".to_string()),
            timestamp: "2026-03-20T01:00:00Z".to_string(),
            goal: "Analyze logs".to_string(),
            status: RunStatus::Success,
            iterations: 1,
            routing_mode: Some(RoutingMode::Orchestrated),
            outcome: Some("2/2 tasks completed".to_string()),
            response_summary: None,
            task_summaries: vec![TaskSummary {
                task_id: 0,
                description: "Search error logs".to_string(),
                status: TaskStatus::Complete,
                worker: Some("sre".to_string()),
                result_preview: Some("Found 3 error groups".to_string()),
                confidence: Some("high".to_string()),
                failure_category: None,
                error: None,
                error_context: None,
                tool_trace: vec![
                    ToolTraceEntry {
                        tool: "search_logs".to_string(),
                        reasoning: String::new(),
                        duration_ms: 1200,
                        outcome: ToolOutcome::Success { output_bytes: 4096 },
                        artifact_filename: None,
                    },
                    ToolTraceEntry {
                        tool: "submit_result".to_string(),
                        reasoning: String::new(),
                        duration_ms: 50,
                        outcome: ToolOutcome::Success { output_bytes: 256 },
                        artifact_filename: None,
                    },
                ],
                artifacts: vec![],
            }],
            artifact_paths: vec![],
            phase_timings: None,
        };

        let result = build_session_context(&[manifest]);

        assert!(result.contains("Outcome: 2/2 tasks completed"));
        assert!(result.contains("Task 0 [sre] — Complete (high)"));
        assert!(result.contains("Summary: \"Found 3 error groups\""));
        assert!(result.contains("Tool chain: search_logs (1.2s) → submit_result (0.1s)"));
    }

    #[test]
    fn test_build_session_context_with_artifacts() {
        let manifest = RunManifest {
            run_id: "run-artifacts".to_string(),
            session_id: Some("cs_test".to_string()),
            timestamp: "2026-03-20T01:00:00Z".to_string(),
            goal: "Generate report".to_string(),
            status: RunStatus::Success,
            iterations: 1,
            routing_mode: Some(RoutingMode::Orchestrated),
            outcome: None,
            response_summary: None,
            task_summaries: vec![TaskSummary {
                task_id: 1,
                description: "Write summary".to_string(),
                status: TaskStatus::Complete,
                worker: Some("writer".to_string()),
                result_preview: Some("Report complete".to_string()),
                confidence: None,
                failure_category: None,
                error: None,
                error_context: None,
                tool_trace: vec![],
                artifacts: vec![
                    ArtifactEntry {
                        filename: "task-1-writer-iter1-result.txt".to_string(),
                        size_bytes: 2048,
                        kind: ArtifactKind::Result,
                    },
                    ArtifactEntry {
                        filename: "task-1-writer-iter1-search-output.txt".to_string(),
                        size_bytes: 8192,
                        kind: ArtifactKind::ToolOutput {
                            tool_name: "search".to_string(),
                        },
                    },
                ],
            }],
            artifact_paths: vec![],
            phase_timings: None,
        };

        let result = build_session_context(&[manifest]);

        assert!(result.contains("Artifacts: task-1-writer-iter1-result.txt (2048B)"));
        assert!(result.contains("task-1-writer-iter1-search-output.txt (8192B)"));
        assert!(result.contains("run_id=\"run-artifacts\""));
        assert!(result.contains("read_artifact"));
    }

    #[test]
    fn test_build_session_context_failed_with_error_context() {
        let manifest = RunManifest {
            run_id: "run-err".to_string(),
            session_id: Some("cs_test".to_string()),
            timestamp: "2026-03-20T01:00:00Z".to_string(),
            goal: "Query database".to_string(),
            status: RunStatus::Failed,
            iterations: 1,
            routing_mode: Some(RoutingMode::Orchestrated),
            outcome: Some("0/1 tasks completed".to_string()),
            response_summary: None,
            task_summaries: vec![TaskSummary {
                task_id: 0,
                description: "Run SQL query".to_string(),
                status: TaskStatus::Failed,
                worker: Some("db-worker".to_string()),
                result_preview: None,
                confidence: None,
                failure_category: Some(super::super::types::FailureCategory::AgentTimeout),
                error: Some("Timed out after 30s".to_string()),
                error_context: Some(ErrorContext {
                    category: super::super::types::FailureCategory::AgentTimeout,
                    last_tool_call: Some("execute_sql".to_string()),
                    attempt_count: 2,
                    partial_result: Some("Retrieved 50 of 500 rows".to_string()),
                }),
                tool_trace: vec![
                    ToolTraceEntry {
                        tool: "execute_sql".to_string(),
                        reasoning: String::new(),
                        duration_ms: 15000,
                        outcome: ToolOutcome::Success { output_bytes: 1024 },
                        artifact_filename: None,
                    },
                    ToolTraceEntry {
                        tool: "execute_sql".to_string(),
                        reasoning: String::new(),
                        duration_ms: 30000,
                        outcome: ToolOutcome::Error {
                            message: "timeout".to_string(),
                        },
                        artifact_filename: None,
                    },
                ],
                artifacts: vec![],
            }],
            artifact_paths: vec![],
            phase_timings: None,
        };

        let result = build_session_context(&[manifest]);

        assert!(result.contains("FAILED (agent_timeout)"));
        assert!(result.contains("Error: Timed out after 30s"));
        assert!(result.contains("Last tool: execute_sql"));
        assert!(result.contains("Partial progress: Retrieved 50 of 500 rows"));
        assert!(result.contains("Tool chain: execute_sql (15.0s) → execute_sql (FAILED: timeout)"));
    }

    #[test]
    fn test_build_session_context_direct_response() {
        let manifest = RunManifest {
            run_id: "run-direct".to_string(),
            session_id: Some("cs_test".to_string()),
            timestamp: "2026-03-20T01:00:00Z".to_string(),
            goal: "What is 2+2?".to_string(),
            status: RunStatus::Success,
            iterations: 0,
            routing_mode: Some(RoutingMode::DirectAnswer),
            outcome: Some("Answered directly".to_string()),
            response_summary: Some("The answer is 4.".to_string()),
            task_summaries: vec![],
            artifact_paths: vec![],
            phase_timings: None,
        };

        let result = build_session_context(&[manifest]);

        assert!(result.contains("Outcome: Answered directly"));
        assert!(result.contains("Response: \"The answer is 4.\""));
        assert!(!result.contains("Tasks:"));
        assert!(!result.contains("run_id=\"run-direct\""));
    }

    #[test]
    fn test_build_session_context_no_artifact_hint_when_empty() {
        let manifests = vec![make_test_manifest(
            "run-1",
            "2026-03-20T01:00:00Z",
            "Simple query",
        )];

        let result = build_session_context(&manifests);

        assert!(!result.contains("run_id=\"run-1\""));
    }

    // ========================================================================
    // Filename Sanitization Tests
    // ========================================================================

    #[test]
    fn test_sanitize_filename_component_normal() {
        assert_eq!(sanitize_filename_component("research"), "research");
        assert_eq!(sanitize_filename_component("sre"), "sre");
    }

    #[test]
    fn test_sanitize_filename_component_special_chars() {
        assert_eq!(sanitize_filename_component("my worker"), "my-worker");
        assert_eq!(sanitize_filename_component("sre/ops"), "sre-ops");
        assert_eq!(sanitize_filename_component("a..b"), "a-b");
        assert_eq!(sanitize_filename_component("UPPER_case"), "upper-case");
    }

    #[test]
    fn test_sanitize_filename_component_empty() {
        assert_eq!(sanitize_filename_component(""), "unknown");
        assert_eq!(sanitize_filename_component("///"), "unknown");
        assert_eq!(sanitize_filename_component("..."), "unknown");
    }

    #[test]
    fn test_sanitize_filename_component_collapse() {
        assert_eq!(sanitize_filename_component("a---b"), "a-b");
        assert_eq!(sanitize_filename_component("--leading"), "leading");
        assert_eq!(sanitize_filename_component("trailing--"), "trailing");
    }

    // ========================================================================
    // Namespaced Artifact Filename Tests
    // ========================================================================

    #[tokio::test]
    async fn test_artifact_filename_includes_worker_and_iteration() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        let filename = persistence
            .write_result_artifact(0, Some("sre"), 2, "content")
            .await
            .unwrap();
        assert_eq!(filename, "task-0-sre-iter-2-result.txt");

        let content = persistence.read_artifact(&filename).await.unwrap();
        assert_eq!(content, "content");
    }

    #[tokio::test]
    async fn test_artifact_filename_default_worker() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        let filename = persistence
            .write_result_artifact(3, None, 1, "content")
            .await
            .unwrap();
        assert_eq!(filename, "task-3-default-iter-1-result.txt");
    }

    // ========================================================================
    // Tool Output Artifact Tests
    // ========================================================================

    #[tokio::test]
    async fn test_write_tool_output_artifact() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        let filename = persistence
            .write_tool_output_artifact(0, "sre", 1, "log_search", 0, "search results here")
            .await
            .unwrap();
        assert_eq!(filename, "task-0-sre-iter-1-log-search-0-output.txt");

        let content = persistence.read_artifact(&filename).await.unwrap();
        assert_eq!(content, "search results here");
    }

    #[tokio::test]
    async fn test_write_tool_output_artifact_sanitizes_names() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        let filename = persistence
            .write_tool_output_artifact(2, "SRE/Ops", 1, "My Search Tool", 3, "data")
            .await
            .unwrap();
        assert_eq!(
            filename,
            "task-2-sre-ops-iter-1-my-search-tool-3-output.txt"
        );
    }

    #[tokio::test]
    async fn test_write_tool_output_artifact_disabled() {
        let persistence = ExecutionPersistence::disabled();
        let filename = persistence
            .write_tool_output_artifact(0, "w", 1, "t", 0, "data")
            .await
            .unwrap();
        assert!(filename.is_empty());
    }

    // ========================================================================
    // Drain Barrier Tests
    // ========================================================================

    #[tokio::test]
    async fn test_drain_completes_immediately_with_no_in_flight() {
        let persistence = ExecutionPersistence::disabled();
        assert!(persistence.drain(Duration::from_millis(100)).await);
    }

    #[tokio::test]
    async fn test_drain_waits_for_in_flight_write() {
        let persistence = ExecutionPersistence::disabled();
        let counter = persistence.in_flight_counter();
        let notify = persistence.drain_notify();

        counter.fetch_add(1, Ordering::Release);

        let drain_handle = {
            let persistence = persistence.clone();
            tokio::spawn(async move { persistence.drain(Duration::from_secs(5)).await })
        };

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!drain_handle.is_finished());

        counter.fetch_sub(1, Ordering::Release);
        notify.notify_one();

        assert!(drain_handle.await.unwrap());
    }

    #[tokio::test]
    async fn test_drain_times_out() {
        let persistence = ExecutionPersistence::disabled();
        let counter = persistence.in_flight_counter();
        counter.fetch_add(1, Ordering::Release);

        let drained = persistence.drain(Duration::from_millis(50)).await;
        assert!(!drained);
    }

    // ========================================================================
    // Hierarchical Manifest Tests (C1)
    // ========================================================================

    #[test]
    fn test_manifest_backward_compat_missing_new_fields() {
        let old_json = r#"{
            "run_id": "run-old",
            "session_id": "cs_old",
            "timestamp": "2026-03-19T12:00:00Z",
            "goal": "Old run",
            "status": "success",
            "iterations": 1,
            "routing_mode": "orchestrated",
            "task_summaries": [{
                "task_id": 0,
                "description": "Old task",
                "status": "complete",
                "worker": "w1",
                "result_preview": "done"
            }],
            "artifact_paths": ["task-0-w1-iter-1-result.txt"]
        }"#;

        let manifest: RunManifest = serde_json::from_str(old_json).unwrap();
        assert_eq!(manifest.run_id, "run-old");
        assert_eq!(manifest.task_summaries.len(), 1);
        let ts = &manifest.task_summaries[0];
        assert!(ts.error.is_none());
        assert!(ts.error_context.is_none());
        assert!(ts.tool_trace.is_empty());
        assert!(ts.artifacts.is_empty());
        assert!(ts.failure_category.is_none());
        assert!(ts.confidence.is_none());
        assert!(manifest.outcome.is_none());
        assert!(manifest.response_summary.is_none());
    }

    #[test]
    fn test_manifest_serde_with_enriched_fields() {
        use super::super::types::FailureCategory;

        let manifest = RunManifest {
            run_id: "run-enriched".to_string(),
            session_id: Some("cs_test".to_string()),
            timestamp: "2026-04-30T12:00:00Z".to_string(),
            goal: "Enriched test".to_string(),
            status: RunStatus::PartialSuccess,
            iterations: 2,
            routing_mode: Some(RoutingMode::Orchestrated),
            outcome: Some("1/2 tasks completed".to_string()),
            response_summary: None,
            task_summaries: vec![
                TaskSummary {
                    task_id: 0,
                    description: "Search logs".to_string(),
                    status: TaskStatus::Complete,
                    worker: Some("sre".to_string()),
                    result_preview: Some("Found 47 errors".to_string()),
                    confidence: Some("high".to_string()),
                    failure_category: None,
                    error: None,
                    error_context: None,
                    tool_trace: vec![ToolTraceEntry {
                        tool: "log_search".to_string(),
                        reasoning: "Searching for errors".to_string(),
                        duration_ms: 8200,
                        outcome: ToolOutcome::Success {
                            output_bytes: 48291,
                        },
                        artifact_filename: Some(
                            "task-0-sre-iter-1-log-search-0-output.txt".to_string(),
                        ),
                    }],
                    artifacts: vec![
                        ArtifactEntry {
                            filename: "task-0-sre-iter-1-result.txt".to_string(),
                            size_bytes: 3200,
                            kind: ArtifactKind::Result,
                        },
                        ArtifactEntry {
                            filename: "task-0-sre-iter-1-log-search-0-output.txt".to_string(),
                            size_bytes: 48291,
                            kind: ArtifactKind::ToolOutput {
                                tool_name: "log-search".to_string(),
                            },
                        },
                    ],
                },
                TaskSummary {
                    task_id: 1,
                    description: "Query deployments".to_string(),
                    status: TaskStatus::Failed,
                    worker: Some("sre".to_string()),
                    result_preview: None,
                    confidence: None,
                    failure_category: Some(FailureCategory::AgentError),
                    error: Some("403 Forbidden".to_string()),
                    error_context: Some(ErrorContext {
                        category: FailureCategory::AgentError,
                        last_tool_call: Some("get_deployments".to_string()),
                        attempt_count: 1,
                        partial_result: Some("Staging query succeeded".to_string()),
                    }),
                    tool_trace: vec![
                        ToolTraceEntry {
                            tool: "get_deployments".to_string(),
                            reasoning: "Checking staging".to_string(),
                            duration_ms: 1200,
                            outcome: ToolOutcome::Success { output_bytes: 890 },
                            artifact_filename: None,
                        },
                        ToolTraceEntry {
                            tool: "get_deployments".to_string(),
                            reasoning: "Checking prod".to_string(),
                            duration_ms: 30200,
                            outcome: ToolOutcome::Error {
                                message: "403 Forbidden".to_string(),
                            },
                            artifact_filename: None,
                        },
                    ],
                    artifacts: vec![],
                },
            ],
            artifact_paths: vec!["task-0-sre-iter-1-result.txt".to_string()],
            phase_timings: None,
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let deserialized: RunManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.task_summaries.len(), 2);

        let t0 = &deserialized.task_summaries[0];
        assert_eq!(t0.tool_trace.len(), 1);
        assert_eq!(t0.tool_trace[0].tool, "log_search");
        assert!(matches!(
            t0.tool_trace[0].outcome,
            ToolOutcome::Success {
                output_bytes: 48291
            }
        ));
        assert_eq!(t0.artifacts.len(), 2);
        assert!(matches!(t0.artifacts[0].kind, ArtifactKind::Result));

        let t1 = &deserialized.task_summaries[1];
        assert_eq!(t1.error.as_deref(), Some("403 Forbidden"));
        assert_eq!(
            t1.error_context.as_ref().unwrap().last_tool_call.as_deref(),
            Some("get_deployments")
        );
        assert_eq!(t1.tool_trace.len(), 2);
        assert!(matches!(
            t1.tool_trace[1].outcome,
            ToolOutcome::Error { .. }
        ));
    }

    #[test]
    fn test_tool_outcome_serde() {
        let success = ToolOutcome::Success { output_bytes: 1234 };
        let json = serde_json::to_string(&success).unwrap();
        assert!(json.contains("output_bytes"));
        let deserialized: ToolOutcome = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            ToolOutcome::Success { output_bytes: 1234 }
        ));

        let error = ToolOutcome::Error {
            message: "timeout".to_string(),
        };
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: ToolOutcome = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, ToolOutcome::Error { .. }));
    }

    #[test]
    fn test_artifact_kind_serde() {
        let result_kind = ArtifactKind::Result;
        let json = serde_json::to_string(&result_kind).unwrap();
        assert_eq!(json, "\"result\"");

        let tool_kind = ArtifactKind::ToolOutput {
            tool_name: "log_search".to_string(),
        };
        let json = serde_json::to_string(&tool_kind).unwrap();
        assert!(json.contains("tool_name"));
        let deserialized: ArtifactKind = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, ArtifactKind::ToolOutput { tool_name } if tool_name == "log_search")
        );
    }

    fn trace_entry(tool: &str, duration_ms: u64) -> ToolTraceEntry {
        ToolTraceEntry {
            tool: tool.to_string(),
            reasoning: "Searching for errors".to_string(),
            duration_ms,
            outcome: ToolOutcome::Success { output_bytes: 8 },
            artifact_filename: None,
        }
    }

    #[tokio::test]
    async fn test_tool_traces_for_task() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        persistence.record_tool_trace(0, trace_entry("log_search", 1500));

        let traces = persistence.tool_traces_for_task(0);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].tool, "log_search");
        assert_eq!(traces[0].duration_ms, 1500);

        let empty = persistence.tool_traces_for_task(99);
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_record_tool_trace_accumulates_across_clones() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        // Traces recorded through a clone (as PersistenceWrapper holds one)
        // must be visible to the original.
        let clone = persistence.clone();
        persistence.record_tool_trace(0, trace_entry("log_search", 10));
        clone.record_tool_trace(0, trace_entry("log_search", 20));

        let traces = persistence.tool_traces_for_task(0);
        assert_eq!(traces.len(), 2);
    }

    #[tokio::test]
    async fn test_record_tool_trace_noop_when_disabled() {
        let persistence = ExecutionPersistence::disabled();
        persistence.record_tool_trace(0, trace_entry("log_search", 10));
        assert!(persistence.tool_traces_for_task(0).is_empty());
    }

    #[tokio::test]
    async fn test_list_artifacts_with_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        persistence
            .write_result_artifact(0, Some("sre"), 1, "short")
            .await
            .unwrap();
        persistence
            .write_result_artifact(1, Some("sre"), 1, "a longer result here")
            .await
            .unwrap();

        let meta = persistence.list_artifacts_with_metadata().await.unwrap();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta[0].0, "task-0-sre-iter-1-result.txt");
        assert_eq!(meta[0].1, 5); // "short" = 5 bytes
        assert_eq!(meta[1].0, "task-1-sre-iter-1-result.txt");
        assert_eq!(meta[1].1, 20); // "a longer result here" = 20 bytes
    }

    // ========================================================================
    // Parked-Run Checkpoint Tests
    // ========================================================================

    #[tokio::test]
    async fn test_reopen_binds_to_recorded_run_directory() {
        let temp_dir = TempDir::new().unwrap();
        let session_id = "cs_reopen".to_string();
        let original =
            ExecutionPersistence::new(temp_dir.path().join("memory"), Some(session_id.clone()))
                .await
                .unwrap();
        let run_id = original.run_id().to_string();
        original
            .write_result_artifact(0, None, 1, "prior artifact")
            .await
            .unwrap();
        drop(original);

        let reopened =
            ExecutionPersistence::reopen(temp_dir.path().join("memory"), Some(session_id), &run_id)
                .await
                .unwrap();

        assert_eq!(reopened.run_id(), run_id, "the recorded run id is kept");
        assert_eq!(
            reopened.run_path(),
            temp_dir
                .path()
                .join("memory")
                .join("cs_reopen")
                .join(&run_id)
        );
        assert_eq!(reopened.current_iteration(), 1);
        assert_eq!(
            reopened
                .read_artifact("task-0-default-iter-1-result.txt")
                .await
                .unwrap(),
            "prior artifact",
            "a resumed run reads the recorded run's artifacts"
        );

        let filename = reopened
            .write_result_artifact(1, None, 1, "resume artifact")
            .await
            .unwrap();
        assert!(
            reopened
                .run_path()
                .join("artifacts")
                .join(&filename)
                .exists()
        );
    }

    #[tokio::test]
    async fn test_reopen_rejects_traversal_run_id() {
        let temp_dir = TempDir::new().unwrap();
        for bad_id in &["../escape", "foo/bar", "..\\win", ""] {
            let result =
                ExecutionPersistence::reopen(temp_dir.path().join("memory"), None, bad_id).await;
            assert!(result.is_err(), "Should reject run_id: {:?}", bad_id);
            assert_eq!(
                result.err().unwrap().kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
    }

    /// Pruning skips the `parked` checkpoint directory itself and any run
    /// holding a parked document (under either filename), while still
    /// removing plain old runs past the cap.
    #[tokio::test]
    async fn test_prune_skips_parked_directory_and_parked_runs() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir.path().join("memory").join("cs_prune");
        let oldest = "0191e8c0-0000-7000-8000-000000000001";
        let second = "0191e8c0-0aaa-7000-8000-000000000005";
        let parked = "0191e8c0-1111-7000-8000-000000000002";
        let parked_resuming = "0191e8c0-2222-7000-8000-000000000003";
        let current = "0191e8c0-9999-7000-8000-000000000004";
        for run in [oldest, second, parked, parked_resuming, current] {
            tokio::fs::create_dir_all(session_dir.join(run))
                .await
                .unwrap();
        }
        let parked_dir = session_dir.join("parked");
        tokio::fs::create_dir_all(&parked_dir).await.unwrap();
        tokio::fs::write(parked_dir.join(format!("{parked}.json")), "{}")
            .await
            .unwrap();
        tokio::fs::write(
            parked_dir.join(format!("{parked_resuming}.resuming.json")),
            "{}",
        )
        .await
        .unwrap();

        let persistence = ExecutionPersistence::reopen(
            temp_dir.path().join("memory"),
            Some("cs_prune".to_string()),
            current,
        )
        .await
        .unwrap();
        persistence.prune_session_runs(1).await;

        assert!(
            !session_dir.join(oldest).exists() && !session_dir.join(second).exists(),
            "plain old runs past the cap are pruned"
        );
        assert!(
            session_dir.join(parked).exists(),
            "a run with a parked document survives pruning"
        );
        assert!(
            session_dir.join(parked_resuming).exists(),
            "a run with only a resuming document survives pruning"
        );
        assert!(
            session_dir.join(current).exists(),
            "the current run survives pruning"
        );
        assert!(
            parked_dir.join(format!("{parked}.json")).exists(),
            "the parked directory's documents are never pruned as runs"
        );
    }

    #[tokio::test]
    async fn test_run_status_parked_serializes_snake_case() {
        let json = serde_json::to_string(&RunStatus::Parked).unwrap();
        assert_eq!(json, r#""parked""#);
        let back: RunStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RunStatus::Parked);
    }
}
