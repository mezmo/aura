//! TodoWrite tool for task planning and progress tracking.
//!
//! This tool allows orchestrator agents to create and manage structured task lists.
//! Plan state is returned as the tool result, making it visible in the conversation
//! history during the ReAct loop.
//!
//! # Design
//!
//! The tool maintains state across the agent's ReAct loop within a single request:
//! - `write_todos` returns full plan state as tool result (agent sees what it wrote)
//! - `read_todos` queries current state without modification
//! - Each write records a new iteration (preserving history for audit)
//! - Filesystem persistence is optional (for debugging/audit trails)
//!
//! **Note**: Cross-session memory (restoring plan state across requests) is future work.
//! Currently, plan visibility relies on tool results in the conversation history.
//!
//! # Iteration Structure
//!
//! ```text
//! PlanState
//! └── iterations: [
//!     Iteration { id: 0, todos: [...], timestamp: ... },
//!     Iteration { id: 1, todos: [...], timestamp: ... },
//!     Iteration { id: 2, todos: [...], timestamp: ... },  <- current (.last())
//! ]
//! ```
//!
//! # Example
//!
//! ```ignore
//! let (tool, state) = TodoWriteTool::new();
//! // Agent calls write_todos with new todo list
//! // Tool returns formatted plan state as result
//! // Agent sees this in conversation history on next turn
//! ```

use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

/// Type alias for plan state event callback.
pub type PlanStateCallback = Arc<dyn Fn(&PlanState) + Send + Sync>;

/// Tool description loaded at compile time.
pub const TODO_TOOL_DESCRIPTION: &str = include_str!("prompts/todo_tool.md");

/// System prompt addition for todo guidance.
pub const TODO_SYSTEM_PROMPT: &str = include_str!("prompts/todo_system.md");

/// Status of a todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoStatus::Pending => write!(f, "pending"),
            TodoStatus::InProgress => write!(f, "in_progress"),
            TodoStatus::Completed => write!(f, "completed"),
        }
    }
}

/// A single todo item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    /// The content/description of the todo item.
    pub content: String,
    /// The current status of the todo item.
    pub status: TodoStatus,
}

/// A single iteration (snapshot) of the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanIteration {
    /// Iteration number (0-indexed).
    pub id: usize,
    /// The todos at this iteration.
    pub todos: Vec<Todo>,
    /// Unix timestamp (milliseconds) when this iteration was created.
    pub timestamp_ms: u64,
}

/// State tracking all plan iterations for audit trail.
///
/// Each call to `write_todos` creates a new iteration, preserving
/// the history of plan changes. This enables:
/// - Audit trail of planning decisions
/// - Rollback capability (future)
/// - Analysis of agent planning behavior
///
/// # Filesystem Structure
///
/// When a `plan_dir` is configured, iterations are persisted:
/// ```text
/// <plan_dir>/
/// ├── iterations/
/// │   ├── 0.json
/// │   ├── 1.json
/// │   └── 2.json
/// └── current -> iterations/2.json
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanState {
    /// All iterations of the plan (newest at the end).
    pub iterations: Vec<PlanIteration>,

    /// Optional directory for persisting iterations to filesystem.
    /// When set, each iteration is written to `<plan_dir>/iterations/<id>.json`
    /// and a `current` symlink points to the latest iteration.
    #[serde(skip)]
    pub plan_dir: Option<PathBuf>,
}

impl PlanState {
    /// Create a new empty plan state (in-memory only).
    pub fn new() -> Self {
        Self {
            iterations: Vec::new(),
            plan_dir: None,
        }
    }

    /// Create a new plan state with filesystem persistence.
    ///
    /// # Arguments
    /// * `plan_dir` - Directory where iterations will be stored
    ///
    /// # Filesystem Structure
    /// ```text
    /// <plan_dir>/
    /// ├── iterations/
    /// │   ├── 0.json
    /// │   └── ...
    /// └── current -> iterations/<latest>.json
    /// ```
    pub fn with_persistence(plan_dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let plan_dir = plan_dir.into();
        let iterations_dir = plan_dir.join("iterations");

        // Create directories if they don't exist
        std::fs::create_dir_all(&iterations_dir)?;

        Ok(Self {
            iterations: Vec::new(),
            plan_dir: Some(plan_dir),
        })
    }

    /// Get the current iteration number (0-indexed, -1 if no iterations).
    pub fn current_iteration(&self) -> isize {
        if self.iterations.is_empty() {
            -1
        } else {
            (self.iterations.len() - 1) as isize
        }
    }

    /// Get the current todos (from the latest iteration).
    pub fn current_todos(&self) -> &[Todo] {
        self.iterations
            .last()
            .map(|i| i.todos.as_slice())
            .unwrap_or(&[])
    }

    /// Add a new iteration with the given todos.
    ///
    /// If `plan_dir` is configured, also persists to filesystem:
    /// - Writes iteration to `<plan_dir>/iterations/<id>.json`
    /// - Updates `current` symlink to point to latest iteration
    pub fn add_iteration(&mut self, todos: Vec<Todo>) -> &PlanIteration {
        let id = self.iterations.len();
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let iteration = PlanIteration {
            id,
            todos,
            timestamp_ms,
        };

        // Persist to filesystem if configured
        if let Some(ref plan_dir) = self.plan_dir
            && let Err(e) = self.persist_iteration(plan_dir, &iteration)
        {
            tracing::warn!("Failed to persist iteration {}: {}", id, e);
        }

        self.iterations.push(iteration);
        self.iterations.last().unwrap()
    }

    /// Persist an iteration to the filesystem.
    ///
    /// Writes to `<plan_dir>/iterations/<id>.json` and updates the `current` symlink.
    fn persist_iteration(&self, plan_dir: &Path, iteration: &PlanIteration) -> std::io::Result<()> {
        let iterations_dir = plan_dir.join("iterations");
        let iteration_file = iterations_dir.join(format!("{}.json", iteration.id));
        let current_symlink = plan_dir.join("current");

        // Write iteration file
        let json = serde_json::to_string_pretty(iteration)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&iteration_file, json)?;

        // Update current symlink (relative path for portability)
        let relative_target = Path::new("iterations").join(format!("{}.json", iteration.id));

        // Remove existing symlink if present
        if current_symlink.exists() || current_symlink.is_symlink() {
            std::fs::remove_file(&current_symlink)?;
        }

        // Create new symlink
        #[cfg(unix)]
        std::os::unix::fs::symlink(&relative_target, &current_symlink)?;

        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&relative_target, &current_symlink)?;

        tracing::debug!(
            "Persisted iteration {} to {:?}, current -> {:?}",
            iteration.id,
            iteration_file,
            relative_target
        );

        Ok(())
    }

    /// Get the path to the iterations directory (if persistence is enabled).
    pub fn iterations_dir(&self) -> Option<PathBuf> {
        self.plan_dir.as_ref().map(|p| p.join("iterations"))
    }

    /// Get the path to the current symlink (if persistence is enabled).
    pub fn current_path(&self) -> Option<PathBuf> {
        self.plan_dir.as_ref().map(|p| p.join("current"))
    }

    /// Format the current state for the agent to see.
    ///
    /// Returns a structured representation showing:
    /// - Current iteration number
    /// - All current todos with their statuses
    /// - Summary statistics
    pub fn format_for_agent(&self) -> String {
        if self.iterations.is_empty() {
            return "Plan state: empty (no iterations yet)".to_string();
        }

        let current = self.iterations.last().unwrap();
        let todos = &current.todos;

        let pending = todos
            .iter()
            .filter(|t| t.status == TodoStatus::Pending)
            .count();
        let in_progress = todos
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        let completed = todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();

        let mut output = format!(
            "Plan state (iteration {}):\n\
             Summary: {} total, {} pending, {} in progress, {} completed\n\n\
             Current todos:\n",
            current.id,
            todos.len(),
            pending,
            in_progress,
            completed
        );

        for (i, todo) in todos.iter().enumerate() {
            let status_icon = match todo.status {
                TodoStatus::Pending => "○",
                TodoStatus::InProgress => "◐",
                TodoStatus::Completed => "●",
            };
            output.push_str(&format!(
                "  {}. [{}] {} ({})\n",
                i + 1,
                status_icon,
                todo.content,
                todo.status
            ));
        }

        output
    }
}

/// Arguments for the write_todos tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTodosArgs {
    /// The complete list of todos (replaces existing list).
    pub todos: Vec<Todo>,
}

/// Shared state for plan iterations across the agent's execution.
pub type TodoState = Arc<RwLock<PlanState>>;

/// Error type for todo operations.
#[derive(Debug, thiserror::Error)]
pub enum TodoError {
    #[error("Invalid todo list: {0}")]
    InvalidTodoList(String),
}

// ============================================================================
// ReadTodos Tool - Allows agent to query current plan state
// ============================================================================

/// ReadTodos tool for querying the current plan state.
///
/// This tool allows agents to see the current plan without modifying it.
/// It's useful for checking progress before deciding on next actions.
#[derive(Clone)]
pub struct ReadTodosTool {
    /// Shared state for plan iterations.
    state: TodoState,
}

impl std::fmt::Debug for ReadTodosTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadTodosTool")
            .field("state", &self.state)
            .finish()
    }
}

impl ReadTodosTool {
    /// Create a ReadTodosTool with existing state (typically shared with TodoWriteTool).
    pub fn with_state(state: TodoState) -> Self {
        Self { state }
    }
}

/// Arguments for the read_todos tool (empty - no parameters needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadTodosArgs {}

impl Tool for ReadTodosTool {
    const NAME: &'static str = "read_todos";
    type Error = TodoError;
    type Args = ReadTodosArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read the current todo list without modifying it. Use this to check \
                          the current plan state, see which tasks are pending/in_progress/completed, \
                          and decide what to do next. Returns the full plan state including \
                          iteration number, task statuses, and summary statistics."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let state = self.state.read().await;
        Ok(state.format_for_agent())
    }
}

/// TodoWrite tool for task planning and progress tracking.
///
/// This tool stores todos in shared state with iteration tracking,
/// and returns the full plan state so the agent can see what it wrote.
/// This enables the agent to replan and iterate based on current state.
#[derive(Clone)]
pub struct TodoWriteTool {
    /// Shared state for plan iterations.
    state: TodoState,
    /// Optional callback for emitting events when todos change.
    event_callback: Option<PlanStateCallback>,
}

impl std::fmt::Debug for TodoWriteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TodoWriteTool")
            .field("state", &self.state)
            .field("event_callback", &self.event_callback.is_some())
            .finish()
    }
}

impl TodoWriteTool {
    /// Create a new TodoWriteTool with fresh state (in-memory only).
    ///
    /// Returns the tool and a handle to the shared state.
    pub fn new() -> (Self, TodoState) {
        let state = Arc::new(RwLock::new(PlanState::new()));
        let tool = Self {
            state: state.clone(),
            event_callback: None,
        };
        (tool, state)
    }

    /// Create a new TodoWriteTool with filesystem persistence.
    ///
    /// Each iteration will be written to `<plan_dir>/iterations/<id>.json`
    /// with a `current` symlink pointing to the latest iteration.
    ///
    /// Returns the tool and a handle to the shared state, or an error if
    /// the directory cannot be created.
    pub fn new_with_persistence(
        plan_dir: impl Into<std::path::PathBuf>,
    ) -> std::io::Result<(Self, TodoState)> {
        let plan_state = PlanState::with_persistence(plan_dir)?;
        let state = Arc::new(RwLock::new(plan_state));
        let tool = Self {
            state: state.clone(),
            event_callback: None,
        };
        Ok((tool, state))
    }

    /// Create a TodoWriteTool with existing state.
    pub fn with_state(state: TodoState) -> Self {
        Self {
            state,
            event_callback: None,
        }
    }

    /// Set a callback to be invoked when todos are updated.
    ///
    /// The callback receives the full `PlanState` including all iterations.
    pub fn with_event_callback<F>(mut self, callback: F) -> Self
    where
        F: Fn(&PlanState) + Send + Sync + 'static,
    {
        self.event_callback = Some(Arc::new(callback));
        self
    }

    /// Get a snapshot of the current todos (from the latest iteration).
    pub async fn get_todos(&self) -> Vec<Todo> {
        self.state.read().await.current_todos().to_vec()
    }

    /// Get a snapshot of the full plan state including all iterations.
    pub async fn get_plan_state(&self) -> PlanState {
        self.state.read().await.clone()
    }

    /// Get the current iteration number.
    pub async fn current_iteration(&self) -> isize {
        self.state.read().await.current_iteration()
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new().0
    }
}

impl Tool for TodoWriteTool {
    const NAME: &'static str = "write_todos";
    type Error = TodoError;
    type Args = WriteTodosArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: TODO_TOOL_DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete list of todos (replaces existing list)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "The content/description of the todo item"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "The current status of the todo item"
                                }
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Validate todos
        for todo in &args.todos {
            if todo.content.trim().is_empty() {
                return Err(TodoError::InvalidTodoList(
                    "Todo content cannot be empty".to_string(),
                ));
            }
        }

        // Add new iteration and get formatted state for response
        let formatted_state = {
            let mut state = self.state.write().await;

            // Add as a new iteration (preserves history for audit)
            let iteration = state.add_iteration(args.todos.clone());

            tracing::info!(
                "📋 Plan iteration {}: {} items ({} in progress)",
                iteration.id,
                iteration.todos.len(),
                iteration
                    .todos
                    .iter()
                    .filter(|t| t.status == TodoStatus::InProgress)
                    .count()
            );

            // Format for agent response
            state.format_for_agent()
        };

        // Emit event if callback is set (with full state for audit)
        if let Some(ref callback) = self.event_callback {
            let state = self.state.read().await;
            callback(&state);
        }

        // Return full formatted state so agent can see what it wrote
        Ok(formatted_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_todo_write_basic() {
        let (tool, state) = TodoWriteTool::new();

        let args = WriteTodosArgs {
            todos: vec![
                Todo {
                    content: "First task".to_string(),
                    status: TodoStatus::InProgress,
                },
                Todo {
                    content: "Second task".to_string(),
                    status: TodoStatus::Pending,
                },
            ],
        };

        let result = tool.call(args).await.unwrap();
        // Response now includes full state
        assert!(result.contains("2 total"));
        assert!(result.contains("1 pending"));
        assert!(result.contains("1 in progress"));
        assert!(result.contains("iteration 0")); // First iteration

        let plan_state = state.read().await;
        assert_eq!(plan_state.iterations.len(), 1);
        assert_eq!(plan_state.current_todos().len(), 2);
    }

    #[tokio::test]
    async fn test_todo_write_creates_iterations() {
        let (tool, state) = TodoWriteTool::new();

        // First write creates iteration 0
        let args1 = WriteTodosArgs {
            todos: vec![Todo {
                content: "Task A".to_string(),
                status: TodoStatus::Pending,
            }],
        };
        let result1 = tool.call(args1).await.unwrap();
        assert!(result1.contains("iteration 0"));

        // Second write creates iteration 1 (preserves history)
        let args2 = WriteTodosArgs {
            todos: vec![
                Todo {
                    content: "Task B".to_string(),
                    status: TodoStatus::InProgress,
                },
                Todo {
                    content: "Task C".to_string(),
                    status: TodoStatus::Completed,
                },
            ],
        };
        let result2 = tool.call(args2).await.unwrap();
        assert!(result2.contains("iteration 1"));

        let plan_state = state.read().await;
        // Both iterations preserved
        assert_eq!(plan_state.iterations.len(), 2);

        // First iteration had 1 task
        assert_eq!(plan_state.iterations[0].todos.len(), 1);
        assert_eq!(plan_state.iterations[0].todos[0].content, "Task A");

        // Current (iteration 1) has 2 tasks
        assert_eq!(plan_state.current_todos().len(), 2);
        assert_eq!(plan_state.current_todos()[0].content, "Task B");
        assert_eq!(plan_state.current_todos()[1].content, "Task C");
    }

    #[tokio::test]
    async fn test_todo_write_empty_content_rejected() {
        let (tool, _state) = TodoWriteTool::new();

        let args = WriteTodosArgs {
            todos: vec![Todo {
                content: "   ".to_string(),
                status: TodoStatus::Pending,
            }],
        };

        let result = tool.call(args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_todo_write_clear_list() {
        let (tool, state) = TodoWriteTool::new();

        // Add todos
        let args1 = WriteTodosArgs {
            todos: vec![Todo {
                content: "Task".to_string(),
                status: TodoStatus::Pending,
            }],
        };
        tool.call(args1).await.unwrap();

        // Clear todos (creates new iteration with empty list)
        let args2 = WriteTodosArgs { todos: vec![] };
        let result = tool.call(args2).await.unwrap();
        assert!(result.contains("empty") || result.contains("0 total"));

        let plan_state = state.read().await;
        // Both iterations preserved (one with tasks, one empty)
        assert_eq!(plan_state.iterations.len(), 2);
        assert!(plan_state.current_todos().is_empty());
        // But iteration 0 still has the task for audit
        assert_eq!(plan_state.iterations[0].todos.len(), 1);
    }

    #[test]
    fn test_todo_status_display() {
        assert_eq!(TodoStatus::Pending.to_string(), "pending");
        assert_eq!(TodoStatus::InProgress.to_string(), "in_progress");
        assert_eq!(TodoStatus::Completed.to_string(), "completed");
    }

    #[test]
    fn test_prompts_loaded() {
        // Verify compile-time includes work
        assert!(!TODO_TOOL_DESCRIPTION.is_empty());
        assert!(!TODO_SYSTEM_PROMPT.is_empty());
        assert!(
            TODO_TOOL_DESCRIPTION.contains("write_todos") || TODO_TOOL_DESCRIPTION.contains("task")
        );
        assert!(TODO_SYSTEM_PROMPT.contains("write_todos"));
    }

    #[test]
    fn test_plan_state_format_for_agent() {
        let mut state = PlanState::new();

        // Empty state
        assert!(state.format_for_agent().contains("empty"));

        // Add iteration
        state.add_iteration(vec![
            Todo {
                content: "First task".to_string(),
                status: TodoStatus::InProgress,
            },
            Todo {
                content: "Second task".to_string(),
                status: TodoStatus::Pending,
            },
        ]);

        let formatted = state.format_for_agent();
        assert!(formatted.contains("iteration 0"));
        assert!(formatted.contains("2 total"));
        assert!(formatted.contains("First task"));
        assert!(formatted.contains("Second task"));
        assert!(formatted.contains("in_progress"));
        assert!(formatted.contains("pending"));
    }

    #[test]
    fn test_plan_state_current_iteration() {
        let mut state = PlanState::new();

        // No iterations
        assert_eq!(state.current_iteration(), -1);

        // Add one iteration
        state.add_iteration(vec![]);
        assert_eq!(state.current_iteration(), 0);

        // Add another
        state.add_iteration(vec![]);
        assert_eq!(state.current_iteration(), 1);
    }

    #[test]
    fn test_plan_iteration_has_timestamp() {
        let mut state = PlanState::new();
        state.add_iteration(vec![]);

        let iteration = &state.iterations[0];
        assert!(iteration.timestamp_ms > 0);
    }

    #[test]
    fn test_plan_state_persistence() {
        // Create temp directory
        let temp_dir = std::env::temp_dir().join(format!("aura_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir); // Clean up if exists

        // Create plan state with persistence
        let mut state = PlanState::with_persistence(&temp_dir).unwrap();

        // Add first iteration
        state.add_iteration(vec![Todo {
            content: "Task A".to_string(),
            status: TodoStatus::Pending,
        }]);

        // Check iteration file exists
        let iter_0_path = temp_dir.join("iterations/0.json");
        assert!(iter_0_path.exists(), "iterations/0.json should exist");

        // Check current symlink exists and points to correct file
        let current_path = temp_dir.join("current");
        assert!(current_path.is_symlink(), "current should be a symlink");

        // Add second iteration
        state.add_iteration(vec![Todo {
            content: "Task B".to_string(),
            status: TodoStatus::InProgress,
        }]);

        // Check second iteration file
        let iter_1_path = temp_dir.join("iterations/1.json");
        assert!(iter_1_path.exists(), "iterations/1.json should exist");

        // Read symlink target
        let target = std::fs::read_link(&current_path).unwrap();
        assert_eq!(
            target,
            std::path::Path::new("iterations/1.json"),
            "current should point to iterations/1.json"
        );

        // Verify content of iteration file
        let content = std::fs::read_to_string(&iter_1_path).unwrap();
        let iteration: PlanIteration = serde_json::from_str(&content).unwrap();
        assert_eq!(iteration.id, 1);
        assert_eq!(iteration.todos.len(), 1);
        assert_eq!(iteration.todos[0].content, "Task B");

        // Clean up
        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_plan_state_no_persistence_by_default() {
        let state = PlanState::new();
        assert!(state.plan_dir.is_none());
        assert!(state.iterations_dir().is_none());
        assert!(state.current_path().is_none());
    }

    #[tokio::test]
    async fn test_read_todos_empty() {
        let state = Arc::new(RwLock::new(PlanState::new()));
        let tool = ReadTodosTool::with_state(state);

        let result = tool.call(ReadTodosArgs {}).await.unwrap();
        assert!(result.contains("empty"));
    }

    #[tokio::test]
    async fn test_read_todos_sees_written_state() {
        // Create shared state
        let (write_tool, state) = TodoWriteTool::new();
        let read_tool = ReadTodosTool::with_state(state);

        // Write some todos
        let args = WriteTodosArgs {
            todos: vec![
                Todo {
                    content: "Task A".to_string(),
                    status: TodoStatus::InProgress,
                },
                Todo {
                    content: "Task B".to_string(),
                    status: TodoStatus::Pending,
                },
            ],
        };
        write_tool.call(args).await.unwrap();

        // Read should see the same state
        let read_result = read_tool.call(ReadTodosArgs {}).await.unwrap();
        assert!(read_result.contains("iteration 0"));
        assert!(read_result.contains("Task A"));
        assert!(read_result.contains("Task B"));
        assert!(read_result.contains("in_progress"));
        assert!(read_result.contains("pending"));
    }

    #[tokio::test]
    async fn test_read_todos_tracks_iterations() {
        let (write_tool, state) = TodoWriteTool::new();
        let read_tool = ReadTodosTool::with_state(state);

        // First write
        write_tool
            .call(WriteTodosArgs {
                todos: vec![Todo {
                    content: "Task 1".to_string(),
                    status: TodoStatus::Pending,
                }],
            })
            .await
            .unwrap();

        // Second write
        write_tool
            .call(WriteTodosArgs {
                todos: vec![Todo {
                    content: "Task 2".to_string(),
                    status: TodoStatus::InProgress,
                }],
            })
            .await
            .unwrap();

        // Read should show iteration 1 (latest)
        let read_result = read_tool.call(ReadTodosArgs {}).await.unwrap();
        assert!(read_result.contains("iteration 1"));
        assert!(read_result.contains("Task 2"));
        // Should NOT contain Task 1 (that was iteration 0)
        assert!(!read_result.contains("Task 1"));
    }
}
