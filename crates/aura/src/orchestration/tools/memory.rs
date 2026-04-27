//! Read-only coordinator tools for durable orchestration memory.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::orchestration::memory_fs::{MemoryFs, MemoryFsOutput};

#[derive(Clone)]
struct MemoryToolConfig {
    root: PathBuf,
    max_read_bytes: usize,
    max_search_results: usize,
}

impl MemoryToolConfig {
    fn fs(&self, max_search_results: Option<usize>) -> MemoryFs {
        MemoryFs::new(
            &self.root,
            self.max_read_bytes,
            max_search_results.unwrap_or(self.max_search_results),
        )
    }
}

macro_rules! memory_tool {
    ($name:ident) => {
        #[derive(Clone)]
        pub struct $name {
            config: MemoryToolConfig,
        }

        impl $name {
            pub fn new(
                root: impl AsRef<Path>,
                max_read_bytes: usize,
                max_search_results: usize,
            ) -> Self {
                Self {
                    config: MemoryToolConfig {
                        root: root.as_ref().to_path_buf(),
                        max_read_bytes,
                        max_search_results,
                    },
                }
            }
        }
    };
}

memory_tool!(ListMemoriesTool);
memory_tool!(ReadMemoryTool);
memory_tool!(SearchMemoryTool);
memory_tool!(RecentMemoryTool);
memory_tool!(MemoryShellTool);

#[derive(Debug, Deserialize, Serialize)]
pub struct ListMemoriesArgs {
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ReadMemoryArgs {
    pub path: String,
    #[serde(default)]
    pub tail_n: Option<usize>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchMemoryArgs {
    pub query: String,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RecentMemoryArgs {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MemoryShellArgs {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListMemoriesOutput {
    pub memories: Vec<String>,
    pub index: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadMemoryOutput {
    pub path: String,
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct SearchMemoryOutput {
    pub matches: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryToolError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl Tool for ListMemoriesTool {
    const NAME: &'static str = "list_memories";

    type Error = MemoryToolError;
    type Args = ListMemoriesArgs;
    type Output = ListMemoriesOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "List durable Aura memory files using virtual paths under /memory. \
                Read-only; workers cannot access this tool."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "include_inactive": {
                        "type": "boolean",
                        "description": "Reserved for future superseded/inactive memories."
                    }
                }
            }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let fs = self.config.fs(None);
        let listing = fs.find_path("/memory", Some("*.md"), None).await?;
        let index = fs.read_path("/memory/index.md", None).await?;
        Ok(ListMemoriesOutput {
            memories: listing
                .stdout
                .lines()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            index: index.stdout,
            truncated: listing.truncated || index.truncated,
        })
    }
}

impl Tool for ReadMemoryTool {
    const NAME: &'static str = "read_memory";

    type Error = MemoryToolError;
    type Args = ReadMemoryArgs;
    type Output = ReadMemoryOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read one durable Markdown memory file by virtual path. Supports an \
                optional tail_n for recent entries. Output is bounded by max_read_bytes."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Virtual path such as /memory/worker-trace.md"},
                    "tail_n": {"type": "integer", "description": "Optional number of trailing lines to return"},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "Reserved tag filter"}
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let fs = self.config.fs(None);
        let output = if let Some(tail_n) = args.tail_n {
            fs.execute(&format!("tail -n {tail_n} {}", args.path), None)
                .await?
        } else {
            fs.read_path(&args.path, None).await?
        };
        Ok(ReadMemoryOutput {
            path: args.path,
            content: output.stdout,
            truncated: output.truncated,
        })
    }
}

impl Tool for SearchMemoryTool {
    const NAME: &'static str = "search_memory";

    type Error = MemoryToolError;
    type Args = SearchMemoryArgs;
    type Output = SearchMemoryOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Search durable Markdown memory first. Fixed-string search by default; \
                set regex=true only when regex matching is needed."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Virtual paths to search; defaults to /memory"},
                    "case_sensitive": {"type": "boolean", "description": "Defaults to false"},
                    "regex": {"type": "boolean", "description": "Defaults to fixed-string search"},
                    "limit": {"type": "integer", "description": "Optional result cap"}
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let fs = self.config.fs(args.limit);
        let mut matches = Vec::new();
        let mut truncated = false;
        let paths = args.paths.unwrap_or_else(|| vec!["/memory".to_string()]);
        for path in paths {
            let output = fs
                .search_path(&path, &args.query, None, args.case_sensitive, args.regex)
                .await?;
            truncated |= output.truncated;
            matches.extend(output.stdout.lines().map(ToString::to_string));
        }
        Ok(SearchMemoryOutput { matches, truncated })
    }
}

impl Tool for RecentMemoryTool {
    const NAME: &'static str = "recent_memory";

    type Error = MemoryToolError;
    type Args = RecentMemoryArgs;
    type Output = SearchMemoryOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Return newest durable run timeline entries from /memory/event-*.md. \
                Optional worker and status filters are fixed-string filters."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer"},
                    "worker": {"type": "string"},
                    "status": {"type": "string"}
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let limit = args.limit.unwrap_or(self.config.max_search_results);
        let fs = self.config.fs(Some(limit));
        let files = fs.find_path("/memory", Some("event-*.md"), None).await?;
        let mut entries = Vec::new();
        for path in files.stdout.lines() {
            let content = fs.read_path(path, None).await?;
            entries.extend(content.stdout.lines().map(ToString::to_string));
        }
        entries.retain(|line| {
            let worker_match = args
                .worker
                .as_ref()
                .map(|worker| line.contains(worker))
                .unwrap_or(true);
            let status_match = args
                .status
                .as_ref()
                .map(|status| line.contains(status))
                .unwrap_or(true);
            worker_match && status_match && !line.trim().is_empty()
        });
        entries.reverse();
        let truncated = entries.len() > limit;
        entries.truncate(limit);
        Ok(SearchMemoryOutput {
            matches: entries,
            truncated,
        })
    }
}

impl Tool for MemoryShellTool {
    const NAME: &'static str = "memory_shell";

    type Error = MemoryToolError;
    type Args = MemoryShellArgs;
    type Output = MemoryFsOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Run a read-only MemoryFS command. Supported commands: pwd, ls, cat, \
                head, tail, stat, find, grep, and query. No Bash, subprocesses, pipes, redirects, \
                command substitution, or write commands are supported."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Read-only MemoryFS DSL command"},
                    "cwd": {"type": "string", "description": "Optional virtual working directory"}
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let fs = self.config.fs(None);
        Ok(fs.execute(&args.command, args.cwd.as_deref()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &std::path::Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn memory_tools_return_virtual_bounded_results() {
        let tmp = TempDir::new().unwrap();
        write_file(
            &tmp.path().join("memory/index.md"),
            "# Memory Index\n- /memory/event-2026-04-23.md\n",
        );
        write_file(
            &tmp.path().join("memory/event-2026-04-23.md"),
            "- 2026-04-23T00:00:00Z status=success worker=trace remembered Archil mount\n",
        );
        let search = SearchMemoryTool::new(tmp.path(), 1024, 10);
        let output = search
            .call(SearchMemoryArgs {
                query: "Archil".to_string(),
                paths: None,
                case_sensitive: false,
                regex: false,
                limit: None,
            })
            .await
            .unwrap();

        assert_eq!(output.matches.len(), 1);
        assert!(output.matches[0].starts_with("/memory/event-2026-04-23.md:"));
    }

    #[tokio::test]
    async fn memory_shell_rejects_write_syntax() {
        let tmp = TempDir::new().unwrap();
        let shell = MemoryShellTool::new(tmp.path(), 1024, 10);

        let output = shell
            .call(MemoryShellArgs {
                command: "cat /memory/index.md | head".to_string(),
                cwd: None,
            })
            .await
            .unwrap();

        assert_ne!(output.exit_code, 0);
    }

    #[tokio::test]
    async fn memory_tool_definitions_are_named() {
        let tmp = TempDir::new().unwrap();
        let list = ListMemoriesTool::new(tmp.path(), 1024, 10);
        let read = ReadMemoryTool::new(tmp.path(), 1024, 10);
        let search = SearchMemoryTool::new(tmp.path(), 1024, 10);
        let recent = RecentMemoryTool::new(tmp.path(), 1024, 10);
        let shell = MemoryShellTool::new(tmp.path(), 1024, 10);

        assert_eq!(list.definition(String::new()).await.name, "list_memories");
        assert_eq!(read.definition(String::new()).await.name, "read_memory");
        assert_eq!(search.definition(String::new()).await.name, "search_memory");
        assert_eq!(recent.definition(String::new()).await.name, "recent_memory");
        assert_eq!(shell.definition(String::new()).await.name, "memory_shell");
    }
}
