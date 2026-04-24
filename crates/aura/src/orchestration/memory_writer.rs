//! Internal writer for durable Markdown orchestration memory.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use tokio::fs;

use super::persistence::{RunManifest, RunStatus};
use super::types::TaskStatus;

#[derive(Debug, Clone)]
pub struct MemoryWriter {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryWriteError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl MemoryWriter {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub async fn write_run(&self, manifest: &RunManifest) -> Result<(), MemoryWriteError> {
        let memory_dir = self.root.join("memory");
        fs::create_dir_all(&memory_dir).await?;

        let date = manifest
            .timestamp
            .split('T')
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown");

        let event_entry = format!(
            "## [{}] run {}\n\n- status: {}\n- goal: {}\n- iterations: {}\n- tasks: {}\n\n",
            manifest.timestamp,
            manifest.run_id,
            run_status(manifest),
            sanitize_inline(&manifest.goal),
            manifest.iterations,
            manifest.task_summaries.len(),
        );
        self.append_entry(
            memory_dir.join(format!("event-{date}.md")),
            "Event Timeline",
            &event_entry,
        )
        .await?;

        for task in &manifest.task_summaries {
            let worker = sanitize_slug(task.worker.as_deref().unwrap_or("unassigned"));
            match task.status {
                TaskStatus::Complete => {
                    if task.result_preview.is_some() {
                        let entry = format!(
                            "## [{}] task {} from run {}\n\n- status: complete\n- worker: {}\n- description: {}\n- result_preview: {}\n\n",
                            manifest.timestamp,
                            task.task_id,
                            manifest.run_id,
                            worker,
                            sanitize_inline(&task.description),
                            sanitize_inline(task.result_preview.as_deref().unwrap_or(""))
                        );
                        self.append_entry(
                            memory_dir.join(format!("worker-{worker}.md")),
                            &format!("Worker Memory: {worker}"),
                            &entry,
                        )
                        .await?;
                    }
                }
                TaskStatus::Failed => {
                    let entry = format!(
                        "## [{}] task {} from run {}\n\n- status: failed\n- worker: {}\n- description: {}\n- retry_hint: {}\n\n",
                        manifest.timestamp,
                        task.task_id,
                        manifest.run_id,
                        worker,
                        sanitize_inline(&task.description),
                        sanitize_inline(
                            task.result_preview.as_deref().unwrap_or(
                                "Inspect the run manifest and artifacts before retrying."
                            )
                        )
                    );
                    self.append_entry(
                        memory_dir.join(format!("failure-{worker}.md")),
                        &format!("Failure Memory: {worker}"),
                        &entry,
                    )
                    .await?;
                }
                TaskStatus::Pending | TaskStatus::Running => {}
            }
        }

        self.write_index(&memory_dir).await?;
        Ok(())
    }

    async fn append_entry(
        &self,
        path: PathBuf,
        title: &str,
        entry: &str,
    ) -> Result<(), MemoryWriteError> {
        let existing = fs::read_to_string(&path).await.unwrap_or_default();
        let existing_body = strip_frontmatter(&existing);
        let body = if existing_body.trim().is_empty() {
            format!("# {title}\n\n{entry}")
        } else {
            format!("{}{}", ensure_trailing_newline(existing_body), entry)
        };
        let entry_count = count_entries(&body);
        let content = with_frontmatter(&body, entry_count);
        atomic_write(&path, &content).await?;
        Ok(())
    }

    async fn write_index(&self, memory_dir: &Path) -> Result<(), MemoryWriteError> {
        let mut entries = fs::read_dir(memory_dir).await?;
        let mut rows = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some("index.md") {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown.md")
                .to_string();
            let content = fs::read_to_string(&path).await.unwrap_or_default();
            rows.push(format!(
                "- /memory/{name} entries={} updated={}",
                count_entries(strip_frontmatter(&content)),
                updated_now()
            ));
        }
        rows.sort();
        let body = format!("# Memory Index\n\n{}\n", rows.join("\n"));
        let content = with_frontmatter(&body, rows.len());
        atomic_write(&memory_dir.join("index.md"), &content).await?;
        Ok(())
    }
}

fn run_status(manifest: &RunManifest) -> &'static str {
    match manifest.status {
        RunStatus::Success => "success",
        RunStatus::PartialSuccess => "partial_success",
        RunStatus::Failed => "failed",
    }
}

fn with_frontmatter(body: &str, entry_count: usize) -> String {
    format!(
        "---\nupdated: {}\nentry_count: {}\nneeds_compact: false\n---\n\n{}",
        updated_now(),
        entry_count,
        body.trim_start()
    )
}

fn updated_now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn strip_frontmatter(content: &str) -> &str {
    if let Some(rest) = content.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        return &rest[end + "\n---\n".len()..];
    }
    content
}

fn ensure_trailing_newline(value: &str) -> String {
    if value.ends_with('\n') {
        value.to_string()
    } else {
        format!("{value}\n")
    }
}

fn count_entries(body: &str) -> usize {
    body.lines().filter(|line| line.starts_with("## [")).count()
}

fn sanitize_inline(value: &str) -> String {
    value.replace('\n', " ").trim().to_string()
}

fn sanitize_slug(value: &str) -> String {
    let slug: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "unassigned".to_string()
    } else {
        slug.to_string()
    }
}

async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let timestamp = DateTime::<Utc>::from(std::time::SystemTime::now())
        .timestamp_nanos_opt()
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{timestamp}"));
    fs::write(&tmp, content).await?;
    fs::rename(tmp, path).await
}

#[cfg(test)]
mod tests {
    use super::super::persistence::{RunStatus, TaskSummary};
    use super::super::types::TaskStatus;
    use super::*;
    use tempfile::TempDir;

    fn manifest() -> RunManifest {
        RunManifest {
            run_id: "run-1".to_string(),
            session_id: Some("session-1".to_string()),
            timestamp: "2026-04-23T10:00:00Z".to_string(),
            goal: "Remember Archil mounted memory".to_string(),
            status: RunStatus::PartialSuccess,
            iterations: 2,
            quality_score: None,
            routing_mode: None,
            task_summaries: vec![
                TaskSummary {
                    task_id: 0,
                    description: "Find prior Archil decision".to_string(),
                    status: TaskStatus::Complete,
                    worker: Some("research".to_string()),
                    result_preview: Some("Archil is just a mounted path".to_string()),
                },
                TaskSummary {
                    task_id: 1,
                    description: "Retry failed lookup".to_string(),
                    status: TaskStatus::Failed,
                    worker: Some("trace worker".to_string()),
                    result_preview: Some("Use grep before asking".to_string()),
                },
            ],
            artifact_paths: vec![],
        }
    }

    #[tokio::test]
    async fn successful_run_writes_event_worker_failure_and_index() {
        let tmp = TempDir::new().unwrap();
        let writer = MemoryWriter::new(tmp.path());

        writer.write_run(&manifest()).await.unwrap();

        let event = fs::read_to_string(tmp.path().join("memory/event-2026-04-23.md"))
            .await
            .unwrap();
        let worker = fs::read_to_string(tmp.path().join("memory/worker-research.md"))
            .await
            .unwrap();
        let failure = fs::read_to_string(tmp.path().join("memory/failure-trace-worker.md"))
            .await
            .unwrap();
        let index = fs::read_to_string(tmp.path().join("memory/index.md"))
            .await
            .unwrap();

        assert!(event.contains("Remember Archil mounted memory"));
        assert!(worker.contains("Archil is just a mounted path"));
        assert!(failure.contains("Use grep before asking"));
        assert!(index.contains("/memory/event-2026-04-23.md"));
    }

    #[tokio::test]
    async fn memory_writer_preserves_existing_entries() {
        let tmp = TempDir::new().unwrap();
        let writer = MemoryWriter::new(tmp.path());

        writer.write_run(&manifest()).await.unwrap();
        writer.write_run(&manifest()).await.unwrap();

        let worker = fs::read_to_string(tmp.path().join("memory/worker-research.md"))
            .await
            .unwrap();
        assert_eq!(count_entries(strip_frontmatter(&worker)), 2);
        assert!(worker.contains("entry_count: 2"));
    }
}
