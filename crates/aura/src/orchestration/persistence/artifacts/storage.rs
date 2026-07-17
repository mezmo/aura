use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::fs;
use tokio::sync::{Mutex, MutexGuard, Notify};
use tracing::Instrument;

use crate::orchestration::persistence::{RunManifest, TaskExecutionRecord, ToolCallRecord};
use crate::orchestration::types::Plan;

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

/// True when `s` is safe to use as a single path component.
fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\\') && !s.contains("..")
}

/// Manages execution artifact persistence (async).
#[derive(Clone)]
pub struct ExecutionPersistence {
    pub(crate) base_path: PathBuf,
    run_id: String,
    session_id: Option<String>,
    current_iteration: usize,
    pub(crate) enabled: bool,
    in_flight: Arc<AtomicUsize>,
    drain_notify: Arc<Notify>,
}

impl ExecutionPersistence {
    /// Create new persistence manager with unique run ID.
    pub async fn new<P: AsRef<Path>>(base_path: P, session_id: Option<String>) -> io::Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();

        if let Some(ref sid) = session_id
            && (sid.is_empty() || sid.contains('/') || sid.contains('\\') || sid.contains(".."))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid session_id for persistence path: {:?}", sid),
            ));
        }

        let effective_base = if let Some(ref sid) = session_id {
            base_path.join(sid)
        } else {
            base_path.clone()
        };

        let run_id = uuid::Uuid::now_v7().to_string();
        let run_path = effective_base.join(&run_id);

        fs::create_dir_all(&run_path).await?;

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
        })
    }

    /// Prune oldest run directories if the session exceeds `max_runs`.
    pub async fn prune_session_runs(&self, max_runs: usize) {
        if !self.enabled || max_runs == 0 || self.session_id.is_none() {
            return;
        }

        let session_dir = match self.base_path.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };

        let mut run_dirs: Vec<String> = Vec::new();
        let mut entries = match fs::read_dir(&session_dir).await {
            Ok(e) => e,
            Err(_) => return,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == "latest" || name == self.run_id {
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
    pub fn in_flight_counter(&self) -> Arc<AtomicUsize> {
        self.in_flight.clone()
    }

    /// Arc handle to the drain notification channel.
    pub fn drain_notify(&self) -> Arc<Notify> {
        self.drain_notify.clone()
    }

    /// Wait for all in-flight persistence writes to complete, bounded by `timeout`.
    #[tracing::instrument(
        name = "persistence.drain",
        skip(self, timeout),
        fields(
            timeout_ms = timeout.as_millis() as u64,
            remaining = tracing::field::Empty,
        )
    )]
    pub async fn drain(&self, timeout: Duration) -> bool {
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
                tracing::Span::current().record("remaining", remaining as i64);
                tracing::warn!(remaining, "Persistence drain timed out");
                false
            }
        }
    }

    /// Get iteration directory path (flat, directly under run dir).
    pub fn iteration_path(&self) -> PathBuf {
        self.base_path
            .join(format!("iteration-{}", self.current_iteration))
    }

    /// Build a dot-namespaced filename for a task attempt artifact.
    fn task_attempt_filename(&self, task_id: usize, attempt: usize, suffix: &str) -> String {
        format!("task-{}.attempt-{}.{}", task_id, attempt, suffix)
    }

    /// Write the plan created by coordinator.
    #[tracing::instrument(
        name = "persistence.write_plan",
        skip(self, plan),
        fields(
            iteration = self.current_iteration,
            task_count = plan.tasks.len(),
        )
    )]
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
    #[tracing::instrument(
        name = "persistence.write_planning_phase",
        skip(self, prompt, response),
        fields(
            iteration = self.current_iteration,
            prompt_bytes = prompt.len(),
            response_bytes = response.len(),
        )
    )]
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
    #[tracing::instrument(
        name = "persistence.write_task_execution",
        skip(self, prompt, response, record),
        fields(
            task_id,
            attempt,
            iteration = self.current_iteration,
            prompt_bytes = prompt.len(),
            response_bytes = response.len(),
        )
    )]
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

        let prompt_file = self.task_attempt_filename(task_id, attempt, "prompt.txt");
        let response_file = self.task_attempt_filename(task_id, attempt, "response.txt");
        fs::write(iter_path.join(&prompt_file), prompt).await?;
        fs::write(iter_path.join(&response_file), response).await?;

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

    /// Directory for result artifacts (run-level, not per-iteration).
    pub fn artifacts_path(&self) -> PathBuf {
        self.base_path.join("artifacts")
    }

    /// Write a large result to an artifact file.
    #[tracing::instrument(
        name = "persistence.write_result_artifact",
        skip(self, result, worker_name),
        fields(
            task_id,
            iteration,
            worker = worker_name.unwrap_or("default"),
            result_bytes = result.len(),
        )
    )]
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
    #[tracing::instrument(
        name = "persistence.write_tool_output_artifact",
        skip(self, output),
        fields(
            task_id,
            worker_name,
            iteration,
            tool_name,
            call_idx,
            output_bytes = output.len(),
        )
    )]
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

        let session_dir = self
            .base_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No parent directory"))?;
        let canonical_session = session_dir
            .canonicalize()
            .unwrap_or_else(|_| session_dir.to_path_buf());
        let canonical_artifact = artifact_path
            .canonicalize()
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

    /// List all artifact filenames with file sizes.
    pub async fn list_artifacts_with_metadata(&self) -> io::Result<Vec<(String, u64)>> {
        if !self.enabled {
            return Ok(Vec::new());
        }

        let artifacts_dir = self.artifacts_path();
        if !artifacts_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = fs::read_dir(&artifacts_dir).await?;
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

    /// Load all tool call records for a given task across all iterations.
    pub async fn load_tool_records_for_task(&self, task_id: usize) -> Vec<ToolCallRecord> {
        if !self.enabled {
            return Vec::new();
        }

        let mut all_records = Vec::new();
        let prefix = format!("task-{task_id}.attempt-");

        for iter_num in 1..=self.current_iteration {
            let iter_dir = self.base_path.join(format!("iteration-{iter_num}"));
            if !iter_dir.exists() {
                continue;
            }
            let Ok(mut entries) = fs::read_dir(&iter_dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if !name_str.starts_with(&prefix) || !name_str.ends_with(".tool-calls.json") {
                    continue;
                }
                let Ok(content) = fs::read_to_string(entry.path()).await else {
                    continue;
                };
                if let Ok(records) = serde_json::from_str::<Vec<ToolCallRecord>>(&content) {
                    all_records.extend(records);
                }
            }
        }

        all_records
    }

    /// Get the session ID (if set).
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Write a typed run manifest to `{run_path}/manifest.json`.
    #[tracing::instrument(
        name = "persistence.write_manifest",
        skip(self, manifest),
        fields(
            task_count = manifest.task_summaries.len(),
            artifact_count = manifest.artifact_paths.len(),
        )
    )]
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
    #[tracing::instrument(
        name = "persistence.append_tool_call",
        skip(self, record),
        fields(
            task_id,
            attempt,
            iteration = self.current_iteration,
            tool = record.tool,
            output_bytes = record.output.as_ref().map(|o| o.len()).unwrap_or(0),
        )
    )]
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

        let mut tool_calls: Vec<ToolCallRecord> = if tool_calls_path.exists() {
            let content = fs::read_to_string(&tool_calls_path).await?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Vec::new()
        };

        tool_calls.push(record.clone());

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

/// Acquire the `ExecutionPersistence` lock with a tracing span around the wait.
pub async fn lock_persistence<'a>(
    mutex: &'a Arc<Mutex<ExecutionPersistence>>,
    operation: &'static str,
) -> MutexGuard<'a, ExecutionPersistence> {
    let span = tracing::info_span!("persistence_lock", operation);
    async { mutex.lock().await }.instrument(span).await
}
