//! Prompt journal for orchestration debugging.
//!
//! When `AURA_PROMPT_JOURNAL=1` is set, writes a single human-readable file
//! showing every prompt sent to the LLM across the full orchestration lifecycle.
//! Each entry is labeled by phase and iteration.
//!
//! The journal is written to `{memory_dir}/{run_id}/prompt-journal.md` and is
//! accessible via the `latest` symlink at `{memory_dir}/latest/prompt-journal.md`.

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Records prompts sent to the LLM at each orchestration phase.
///
/// Uses append-per-entry writes with flush for crash safety and `tail -f` support.
pub(crate) struct PromptJournal {
    file: Mutex<std::fs::File>,
}

/// Identifies which orchestration phase a journal entry belongs to.
///
/// Iteration is injected by `record()` (read from `Orchestrator.current_iteration`)
/// rather than stored in each variant.
pub(crate) enum JournalPhase<'a> {
    Planning {
        attempt: usize,
        max_attempts: usize,
    },
    Worker {
        task_id: usize,
        worker_name: Option<&'a str>,
        attempt: usize,
    },
    Synthesis,
    Evaluation,
}

impl<'a> JournalPhase<'a> {
    /// Format the phase header including the iteration number.
    fn display_with_iteration(&self, iteration: usize) -> String {
        match self {
            JournalPhase::Planning {
                attempt,
                max_attempts,
            } => format!(
                "Planning (Iteration {}, Attempt {}/{})",
                iteration, attempt, max_attempts
            ),
            JournalPhase::Worker {
                task_id,
                worker_name: Some(name),
                ..
            } => format!(
                "Worker: {} (Iteration {}, Task {})",
                name, iteration, task_id
            ),
            JournalPhase::Worker {
                task_id,
                worker_name: None,
                ..
            } => format!("Worker (Iteration {}, Task {})", iteration, task_id),
            JournalPhase::Synthesis => format!("Synthesis (Iteration {})", iteration),
            JournalPhase::Evaluation => format!("Evaluation (Iteration {})", iteration),
        }
    }

    /// Returns the relative persistence path where the LLM response can be found.
    fn response_hint(&self) -> String {
        match self {
            JournalPhase::Planning { .. } => "planning/response.txt".to_string(),
            JournalPhase::Worker {
                task_id, attempt, ..
            } => {
                format!("tasks/task-{}/attempt-{}/response.txt", task_id, attempt)
            }
            JournalPhase::Synthesis => "synthesis/response.txt".to_string(),
            JournalPhase::Evaluation => "evaluation/response.txt".to_string(),
        }
    }
}

// Display delegates to display_with_iteration(0) for standalone formatting.
// In practice, `record()` always calls `display_with_iteration` directly.
impl<'a> fmt::Display for JournalPhase<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_with_iteration(0))
    }
}

impl PromptJournal {
    /// Create a new journal file with a header identifying the session.
    pub(crate) fn new(path: PathBuf, run_id: &str, orchestrator_id: &str) -> io::Result<Self> {
        let mut file = std::fs::File::create(&path)?;

        writeln!(file, "# Prompt Journal")?;
        writeln!(file, "Run ID: {}", run_id)?;
        writeln!(file, "Orchestrator ID: {}", orchestrator_id)?;
        writeln!(
            file,
            "Started: {}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
        )?;
        writeln!(file)?;
        file.flush()?;

        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Create a journal inside the persistence run directory if enabled.
    ///
    /// Returns `None` when `journal_enabled` is false, or when file creation fails.
    pub(crate) fn from_persistence(
        persistence_base_path: &Path,
        run_id: &str,
        orchestrator_id: &str,
        journal_enabled: bool,
    ) -> Option<Self> {
        if !journal_enabled {
            return None;
        }

        let path = persistence_base_path.join("prompt-journal.md");
        match Self::new(path, run_id, orchestrator_id) {
            Ok(journal) => {
                tracing::info!(
                    "Prompt journal enabled: {}/prompt-journal.md",
                    persistence_base_path.display()
                );
                Some(journal)
            }
            Err(e) => {
                tracing::warn!("Failed to create prompt journal: {}", e);
                None
            }
        }
    }

    /// Record a prompt entry for the given phase and iteration.
    ///
    /// Acquires the mutex, appends a formatted entry, and flushes.
    pub(crate) fn record(
        &self,
        phase: JournalPhase,
        iteration: usize,
        system_prompt: &str,
        user_prompt: &str,
    ) {
        let Ok(mut file) = self.file.lock() else {
            return;
        };

        let separator = "═".repeat(80);
        let sub_header = |label: &str| format!("── {} {}", label, "─".repeat(64 - label.len()));

        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let phase_display = phase.display_with_iteration(iteration);
        let response_hint = phase.response_hint();

        // Write entry — ignore errors (diagnostic-only, must not break orchestration)
        let _ = (|| -> io::Result<()> {
            writeln!(file, "{}", separator)?;
            writeln!(file, " PHASE: {}", phase_display)?;
            writeln!(file, " TIMESTAMP: {}", timestamp)?;
            writeln!(file, "{}", separator)?;
            writeln!(file)?;
            writeln!(file, "{}", sub_header("SYSTEM PROMPT"))?;
            writeln!(file)?;
            writeln!(file, "{}", system_prompt)?;
            writeln!(file)?;
            writeln!(file, "{}", sub_header("USER PROMPT"))?;
            writeln!(file)?;
            writeln!(file, "{}", user_prompt)?;
            writeln!(file)?;
            writeln!(file, "── RESPONSE: see {} ──", response_hint)?;
            writeln!(file)?;
            file.flush()?;
            Ok(())
        })();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn from_persistence_returns_none_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let result = PromptJournal::from_persistence(tmp.path(), "run-123", "orch-456", false);
        assert!(result.is_none());
    }

    #[test]
    fn from_persistence_returns_some_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let result = PromptJournal::from_persistence(tmp.path(), "run-123", "orch-456", true);
        assert!(result.is_some());
        let journal_path = tmp.path().join("prompt-journal.md");
        assert!(journal_path.exists());
    }

    #[test]
    fn journal_writes_header_with_ids() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let _journal = PromptJournal::new(path.clone(), "run-abc", "orch-xyz").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Prompt Journal"));
        assert!(content.contains("Run ID: run-abc"));
        assert!(content.contains("Orchestrator ID: orch-xyz"));
        assert!(content.contains("Started:"));
    }

    #[test]
    fn journal_records_planning_phase() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();
        journal.record(
            JournalPhase::Planning {
                attempt: 2,
                max_attempts: 3,
            },
            1,
            "You are a coordinator.",
            "Analyze this query: hello",
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Planning (Iteration 1, Attempt 2/3)"));
        assert!(content.contains("You are a coordinator."));
        assert!(content.contains("Analyze this query: hello"));
        assert!(content.contains("planning/response.txt"));
    }

    #[test]
    fn journal_records_worker_phase_with_name() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();
        journal.record(
            JournalPhase::Worker {
                task_id: 3,
                worker_name: Some("arithmetic"),
                attempt: 1,
            },
            2,
            "You are a math worker.",
            "Calculate 2+2",
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Worker: arithmetic (Iteration 2, Task 3)"));
        assert!(content.contains("You are a math worker."));
        assert!(content.contains("Calculate 2+2"));
        assert!(content.contains("tasks/task-3/attempt-1/response.txt"));
    }

    #[test]
    fn journal_records_worker_phase_without_name() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();
        journal.record(
            JournalPhase::Worker {
                task_id: 0,
                worker_name: None,
                attempt: 1,
            },
            1,
            "system",
            "user",
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Worker (Iteration 1, Task 0)"));
    }

    #[test]
    fn journal_records_worker_attempt_in_response_hint() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();
        journal.record(
            JournalPhase::Worker {
                task_id: 5,
                worker_name: Some("stats"),
                attempt: 3,
            },
            1,
            "sys",
            "usr",
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("tasks/task-5/attempt-3/response.txt"));
    }

    #[test]
    fn journal_records_synthesis_and_evaluation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();

        journal.record(JournalPhase::Synthesis, 1, "synth system", "synth user");
        journal.record(JournalPhase::Evaluation, 1, "eval system", "eval user");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Synthesis (Iteration 1)"));
        assert!(content.contains("synthesis/response.txt"));
        assert!(content.contains("Evaluation (Iteration 1)"));
        assert!(content.contains("evaluation/response.txt"));
    }

    #[test]
    fn journal_output_format_has_separators() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();
        journal.record(
            JournalPhase::Planning {
                attempt: 1,
                max_attempts: 3,
            },
            1,
            "sys",
            "usr",
        );

        let content = std::fs::read_to_string(&path).unwrap();
        // Check for separator characters
        assert!(content.contains("═══"));
        assert!(content.contains("PHASE:"));
        assert!(content.contains("TIMESTAMP:"));
        assert!(content.contains("── SYSTEM PROMPT"));
        assert!(content.contains("── USER PROMPT"));
        assert!(content.contains("── RESPONSE: see"));
    }

    #[test]
    fn journal_lifecycle_ordering() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();

        journal.record(
            JournalPhase::Planning {
                attempt: 1,
                max_attempts: 3,
            },
            1,
            "plan-sys",
            "plan-usr",
        );
        journal.record(
            JournalPhase::Worker {
                task_id: 0,
                worker_name: Some("w1"),
                attempt: 1,
            },
            1,
            "worker-sys",
            "worker-usr",
        );
        journal.record(JournalPhase::Synthesis, 1, "synth-sys", "synth-usr");
        journal.record(JournalPhase::Evaluation, 1, "eval-sys", "eval-usr");

        let content = std::fs::read_to_string(&path).unwrap();

        let plan_pos = content.find("Planning").unwrap();
        let worker_pos = content.find("Worker: w1").unwrap();
        let synth_pos = content.find("Synthesis").unwrap();
        let eval_pos = content.find("Evaluation").unwrap();

        assert!(plan_pos < worker_pos, "Planning should come before Worker");
        assert!(
            worker_pos < synth_pos,
            "Worker should come before Synthesis"
        );
        assert!(
            synth_pos < eval_pos,
            "Synthesis should come before Evaluation"
        );
    }

    #[test]
    fn journal_handles_empty_prompts() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prompt-journal.md");

        let journal = PromptJournal::new(path.clone(), "run-1", "orch-1").unwrap();
        journal.record(
            JournalPhase::Planning {
                attempt: 1,
                max_attempts: 1,
            },
            1,
            "",
            "",
        );

        let content = std::fs::read_to_string(&path).unwrap();
        // Format should still be valid with separators present
        assert!(content.contains("PHASE: Planning"));
        assert!(content.contains("── SYSTEM PROMPT"));
        assert!(content.contains("── USER PROMPT"));
        assert!(content.contains("── RESPONSE: see"));
    }
}
