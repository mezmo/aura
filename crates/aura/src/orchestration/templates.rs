//! Prompt template rendering with type-safe validation.
//!
//! This module provides a simple template system using `%%VAR%%` placeholders
//! that avoids conflicts with JSON `{{`/`}}` literals and Rust format strings.
//! Every prompt template in `crates/aura/src/prompts/` is loaded here and
//! rendered through a typed `TemplateVars` impl, so the placeholder convention
//! is uniform across the orchestration pipeline.
//!
//! # Type Safety
//!
//! Each template has an associated context type implementing `TemplateVars`.
//! Tests validate bi-directionally that:
//! - All template placeholders are provided by the context
//! - All context fields are used in the template
//!
//! # Example
//!
//! ```ignore
//! let rendered = render_worker_task_prompt(&WorkerTaskVars {
//!     context: "",
//!     your_task: "List all error entries",
//! });
//! ```

#[cfg(test)]
use std::collections::HashSet;

// Template constants loaded at compile time
pub const WORKER_TASK_PROMPT_TEMPLATE: &str = include_str!("../prompts/worker_task_prompt.md");
pub const CONTINUATION_PROMPT_TEMPLATE: &str = include_str!("../prompts/continuation_prompt.md");
pub const ORCHESTRATOR_PREAMBLE_TEMPLATE: &str =
    include_str!("../prompts/orchestrator_preamble.md");
pub const WORKER_PREAMBLE_TEMPLATE: &str = include_str!("../prompts/worker_preamble.md");
pub const SESSION_HISTORY_TEMPLATE: &str = include_str!("../prompts/session_history.md");
pub const DUPLICATE_CALL_GUIDANCE_TEMPLATE: &str =
    include_str!("../prompts/duplicate_call_guidance.md");
pub const DUPLICATE_CALL_ABORT_TEMPLATE: &str = include_str!("../prompts/duplicate_call_abort.md");
pub const PLANNING_PROMPT_TEMPLATE: &str = include_str!("../prompts/planning_prompt.md");
pub const WORKER_ROSTER_TEMPLATE: &str = include_str!("../prompts/worker_roster.md");
pub const WORKER_GUIDELINES_TEMPLATE: &str = include_str!("../prompts/worker_guidelines.md");
pub const CONTINUATION_WRAPPER_TEMPLATE: &str = include_str!("../prompts/continuation_wrapper.md");

/// Trait for template variable providers.
///
/// Implementing types declare the variable names they provide,
/// enabling compile-time and test-time validation.
pub trait TemplateVars {
    /// Variable names this context provides (uppercase, without %% delimiters).
    /// Used by validation tests to ensure templates and structs stay in sync.
    #[allow(dead_code)]
    const VARS: &'static [&'static str];

    /// Render this context into the given template.
    fn render(&self, template: &str) -> String;
}

/// Variables for the worker task prompt.
#[derive(Debug, Clone)]
pub struct WorkerTaskVars<'a> {
    pub context: &'a str,
    pub your_task: &'a str,
}

impl TemplateVars for WorkerTaskVars<'_> {
    const VARS: &'static [&'static str] = &["CONTEXT", "YOUR_TASK"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%CONTEXT%%", self.context)
            .replace("%%YOUR_TASK%%", self.your_task)
    }
}

/// Variables for the continuation prompt (post-execute decision point).
///
/// Rendered for every post-execute coordinator call — clean success, failure,
/// partial, and blocked-dependency cases all share this template. The
/// coordinator chooses one routing tool to continue, conclude, or clarify.
#[derive(Debug, Clone)]
pub struct ContinuationVars<'a> {
    pub iteration: &'a str,
    pub max_iterations: &'a str,
    pub urgency: &'a str,
    pub succeeded: &'a str,
    pub total: &'a str,
    pub goal: &'a str,
    pub completed_section: &'a str,
    pub blocked_section: &'a str,
    pub redesign_section: &'a str,
    pub failure_section: &'a str,
    pub failure_history: &'a str,
    pub reuse_guidance: &'a str,
}

impl TemplateVars for ContinuationVars<'_> {
    const VARS: &'static [&'static str] = &[
        "ITERATION",
        "MAX_ITERATIONS",
        "URGENCY",
        "SUCCEEDED",
        "TOTAL",
        "GOAL",
        "COMPLETED_SECTION",
        "BLOCKED_SECTION",
        "REDESIGN_SECTION",
        "FAILURE_SECTION",
        "FAILURE_HISTORY",
        "REUSE_GUIDANCE",
    ];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%ITERATION%%", self.iteration)
            .replace("%%MAX_ITERATIONS%%", self.max_iterations)
            .replace("%%URGENCY%%", self.urgency)
            .replace("%%SUCCEEDED%%", self.succeeded)
            .replace("%%TOTAL%%", self.total)
            .replace("%%GOAL%%", self.goal)
            .replace("%%COMPLETED_SECTION%%", self.completed_section)
            .replace("%%BLOCKED_SECTION%%", self.blocked_section)
            .replace("%%REDESIGN_SECTION%%", self.redesign_section)
            .replace("%%FAILURE_SECTION%%", self.failure_section)
            .replace("%%FAILURE_HISTORY%%", self.failure_history)
            .replace("%%REUSE_GUIDANCE%%", self.reuse_guidance)
    }
}

/// Variables for the coordinator system preamble.
#[derive(Debug, Clone)]
pub struct CoordinatorPreambleVars<'a> {
    pub orchestration_system_prompt: &'a str,
    pub tools_section: &'a str,
    pub recon_guidance: &'a str,
}

impl TemplateVars for CoordinatorPreambleVars<'_> {
    const VARS: &'static [&'static str] = &[
        "ORCHESTRATION_SYSTEM_PROMPT",
        "TOOLS_SECTION",
        "RECON_GUIDANCE",
    ];

    fn render(&self, template: &str) -> String {
        template
            .replace(
                "%%ORCHESTRATION_SYSTEM_PROMPT%%",
                self.orchestration_system_prompt,
            )
            .replace("%%TOOLS_SECTION%%", self.tools_section)
            .replace("%%RECON_GUIDANCE%%", self.recon_guidance)
    }
}

/// Variables for the worker preamble (generic-worker fallback).
#[derive(Debug, Clone)]
pub struct WorkerPreambleVars<'a> {
    pub worker_system_prompt: &'a str,
}

impl TemplateVars for WorkerPreambleVars<'_> {
    const VARS: &'static [&'static str] = &["WORKER_SYSTEM_PROMPT"];

    fn render(&self, template: &str) -> String {
        template.replace("%%WORKER_SYSTEM_PROMPT%%", self.worker_system_prompt)
    }
}

/// Variables for the session-history block.
#[derive(Debug, Clone)]
pub struct SessionHistoryVars<'a> {
    pub turn_count: &'a str,
    pub turn_entries: &'a str,
}

impl TemplateVars for SessionHistoryVars<'_> {
    const VARS: &'static [&'static str] = &["TURN_COUNT", "TURN_ENTRIES"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%TURN_COUNT%%", self.turn_count)
            .replace("%%TURN_ENTRIES%%", self.turn_entries)
    }
}

/// Variables for the duplicate-call guard annotations.
#[derive(Debug, Clone)]
pub struct DuplicateCallVars<'a> {
    pub tool_name: &'a str,
    pub count: &'a str,
}

impl TemplateVars for DuplicateCallVars<'_> {
    const VARS: &'static [&'static str] = &["TOOL_NAME", "COUNT"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%TOOL_NAME%%", self.tool_name)
            .replace("%%COUNT%%", self.count)
    }
}

/// Variables for the initial planning wrapper.
#[derive(Debug, Clone)]
pub struct PlanningVars<'a> {
    pub timestamp: &'a str,
    pub query: &'a str,
    pub worker_section: &'a str,
    pub worker_guidelines: &'a str,
}

impl TemplateVars for PlanningVars<'_> {
    const VARS: &'static [&'static str] =
        &["TIMESTAMP", "QUERY", "WORKER_SECTION", "WORKER_GUIDELINES"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%TIMESTAMP%%", self.timestamp)
            .replace("%%QUERY%%", self.query)
            .replace("%%WORKER_SECTION%%", self.worker_section)
            .replace("%%WORKER_GUIDELINES%%", self.worker_guidelines)
    }
}

/// Variables for the worker roster section in the planning wrapper.
#[derive(Debug, Clone)]
pub struct WorkerRosterVars<'a> {
    pub header_note: &'a str,
    pub roster_content: &'a str,
    pub closing_line: &'a str,
}

impl TemplateVars for WorkerRosterVars<'_> {
    const VARS: &'static [&'static str] = &["HEADER_NOTE", "ROSTER_CONTENT", "CLOSING_LINE"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%HEADER_NOTE%%", self.header_note)
            .replace("%%ROSTER_CONTENT%%", self.roster_content)
            .replace("%%CLOSING_LINE%%", self.closing_line)
    }
}

/// Variables for the worker-assignment guidelines in the planning wrapper.
#[derive(Debug, Clone)]
pub struct WorkerGuidelinesVars<'a> {
    pub valid_worker_names: &'a str,
}

impl TemplateVars for WorkerGuidelinesVars<'_> {
    const VARS: &'static [&'static str] = &["VALID_WORKER_NAMES"];

    fn render(&self, template: &str) -> String {
        template.replace("%%VALID_WORKER_NAMES%%", self.valid_worker_names)
    }
}

/// Variables for the continuation timestamp wrapper.
#[derive(Debug, Clone)]
pub struct ContinuationWrapperVars<'a> {
    pub timestamp: &'a str,
    pub continuation_body: &'a str,
}

impl TemplateVars for ContinuationWrapperVars<'_> {
    const VARS: &'static [&'static str] = &["TIMESTAMP", "CONTINUATION_BODY"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%TIMESTAMP%%", self.timestamp)
            .replace("%%CONTINUATION_BODY%%", self.continuation_body)
    }
}

/// Render the worker task prompt with the given variables.
pub fn render_worker_task_prompt(vars: &WorkerTaskVars<'_>) -> String {
    vars.render(WORKER_TASK_PROMPT_TEMPLATE)
}

/// Render the continuation prompt with the given variables.
pub fn render_continuation_prompt(vars: &ContinuationVars<'_>) -> String {
    vars.render(CONTINUATION_PROMPT_TEMPLATE)
}

/// Render the coordinator system preamble with the given variables.
pub fn render_coordinator_preamble(vars: &CoordinatorPreambleVars<'_>) -> String {
    vars.render(ORCHESTRATOR_PREAMBLE_TEMPLATE)
}

/// Render the worker preamble with the given variables.
pub fn render_worker_preamble(vars: &WorkerPreambleVars<'_>) -> String {
    vars.render(WORKER_PREAMBLE_TEMPLATE)
}

/// Render the session-history block with the given variables.
pub fn render_session_history(vars: &SessionHistoryVars<'_>) -> String {
    vars.render(SESSION_HISTORY_TEMPLATE)
}

/// Render the duplicate-call guidance annotation with the given variables.
pub fn render_duplicate_call_guidance(vars: &DuplicateCallVars<'_>) -> String {
    vars.render(DUPLICATE_CALL_GUIDANCE_TEMPLATE)
}

/// Render the duplicate-call abort annotation with the given variables.
pub fn render_duplicate_call_abort(vars: &DuplicateCallVars<'_>) -> String {
    vars.render(DUPLICATE_CALL_ABORT_TEMPLATE)
}

/// Render the initial planning wrapper with the given variables.
pub fn render_planning_prompt(vars: &PlanningVars<'_>) -> String {
    vars.render(PLANNING_PROMPT_TEMPLATE)
}

/// Render the worker roster section with the given variables.
pub fn render_worker_roster(vars: &WorkerRosterVars<'_>) -> String {
    vars.render(WORKER_ROSTER_TEMPLATE)
}

/// Render the worker-assignment guidelines with the given variables.
pub fn render_worker_guidelines(vars: &WorkerGuidelinesVars<'_>) -> String {
    vars.render(WORKER_GUIDELINES_TEMPLATE)
}

/// Render the continuation timestamp wrapper with the given variables.
pub fn render_continuation_wrapper(vars: &ContinuationWrapperVars<'_>) -> String {
    vars.render(CONTINUATION_WRAPPER_TEMPLATE)
}

/// Extract `%%VAR%%` placeholders from a template string.
#[cfg(test)]
fn extract_placeholders(template: &str) -> HashSet<String> {
    let mut vars = HashSet::new();
    let mut i = 0;
    let bytes = template.as_bytes();

    while i < bytes.len().saturating_sub(3) {
        // Look for opening %%
        if bytes[i] == b'%' && bytes[i + 1] == b'%' {
            i += 2;
            let start = i;
            // Find closing %%
            while i < bytes.len().saturating_sub(1) {
                if bytes[i] == b'%' && bytes[i + 1] == b'%' {
                    if i > start
                        && let Ok(var) = std::str::from_utf8(&bytes[start..i])
                    {
                        vars.insert(var.to_string());
                    }
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    vars
}

/// Validate that a template's placeholders match the expected variables.
///
/// Returns `Ok(())` if valid, or `Err` with a description of mismatches.
#[cfg(test)]
fn validate_template<T: TemplateVars>(template: &str) -> Result<(), String> {
    let template_vars = extract_placeholders(template);
    let context_vars: HashSet<_> = T::VARS.iter().map(|s| s.to_string()).collect();

    let mut errors = Vec::new();

    // Check template vars are in context
    for var in &template_vars {
        if !context_vars.contains(var) {
            errors.push(format!(
                "Template has %%{}%% but context doesn't provide it",
                var
            ));
        }
    }

    // Check context vars are in template
    for var in &context_vars {
        if !template_vars.contains(var) {
            errors.push(format!(
                "Context provides {} but template doesn't use %%{}%%",
                var, var
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Render function tests
    // =========================================================================

    #[test]
    fn test_render_basic_substitution() {
        let vars = WorkerTaskVars {
            context: "test context",
            your_task: "test task",
        };
        let rendered = vars.render("Context: %%CONTEXT%%, Task: %%YOUR_TASK%%");
        assert_eq!(rendered, "Context: test context, Task: test task");
    }

    #[test]
    fn test_render_empty_value() {
        let vars = WorkerTaskVars {
            context: "",
            your_task: "test",
        };
        let rendered = vars.render("Start%%CONTEXT%%End");
        assert_eq!(rendered, "StartEnd");
    }

    #[test]
    fn test_render_preserves_json_braces() {
        let vars = WorkerTaskVars {
            context: "test",
            your_task: "test",
        };
        let template = r#"{"task": "%%YOUR_TASK%%", "nested": {"key": "value"}}"#;
        let rendered = vars.render(template);
        assert_eq!(rendered, r#"{"task": "test", "nested": {"key": "value"}}"#);
    }

    // =========================================================================
    // Placeholder extraction tests
    // =========================================================================

    #[test]
    fn test_extract_placeholders_basic() {
        let template = "Hello %%NAME%%, you have %%COUNT%% messages.";
        let vars = extract_placeholders(template);
        assert!(vars.contains("NAME"));
        assert!(vars.contains("COUNT"));
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn test_extract_placeholders_ignores_json() {
        let template = r#"Output: {"key": "%%VALUE%%"}"#;
        let vars = extract_placeholders(template);
        assert!(vars.contains("VALUE"));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn test_extract_placeholders_handles_duplicates() {
        let template = "%%VAR%% and %%VAR%% again";
        let vars = extract_placeholders(template);
        assert!(vars.contains("VAR"));
        assert_eq!(vars.len(), 1);
    }

    // =========================================================================
    // Bi-directional validation tests (template ↔ struct)
    // =========================================================================

    #[test]
    fn test_worker_task_template_matches_context() {
        validate_template::<WorkerTaskVars>(WORKER_TASK_PROMPT_TEMPLATE)
            .expect("Worker task template should match WorkerTaskVars");
    }

    #[test]
    fn test_continuation_template_matches_context() {
        validate_template::<ContinuationVars>(CONTINUATION_PROMPT_TEMPLATE)
            .expect("Continuation template should match ContinuationVars");
    }

    #[test]
    fn test_orchestrator_preamble_template_matches_context() {
        validate_template::<CoordinatorPreambleVars>(ORCHESTRATOR_PREAMBLE_TEMPLATE)
            .expect("Orchestrator preamble template should match CoordinatorPreambleVars");
    }

    #[test]
    fn test_worker_preamble_template_matches_context() {
        validate_template::<WorkerPreambleVars>(WORKER_PREAMBLE_TEMPLATE)
            .expect("Worker preamble template should match WorkerPreambleVars");
    }

    #[test]
    fn test_session_history_template_matches_context() {
        validate_template::<SessionHistoryVars>(SESSION_HISTORY_TEMPLATE)
            .expect("Session history template should match SessionHistoryVars");
    }

    #[test]
    fn test_duplicate_call_guidance_template_matches_context() {
        validate_template::<DuplicateCallVars>(DUPLICATE_CALL_GUIDANCE_TEMPLATE)
            .expect("Duplicate-call guidance template should match DuplicateCallVars");
    }

    #[test]
    fn test_duplicate_call_abort_template_matches_context() {
        validate_template::<DuplicateCallVars>(DUPLICATE_CALL_ABORT_TEMPLATE)
            .expect("Duplicate-call abort template should match DuplicateCallVars");
    }

    #[test]
    fn test_planning_prompt_template_matches_context() {
        validate_template::<PlanningVars>(PLANNING_PROMPT_TEMPLATE)
            .expect("Planning prompt template should match PlanningVars");
    }

    #[test]
    fn test_worker_roster_template_matches_context() {
        validate_template::<WorkerRosterVars>(WORKER_ROSTER_TEMPLATE)
            .expect("Worker roster template should match WorkerRosterVars");
    }

    #[test]
    fn test_worker_guidelines_template_matches_context() {
        validate_template::<WorkerGuidelinesVars>(WORKER_GUIDELINES_TEMPLATE)
            .expect("Worker guidelines template should match WorkerGuidelinesVars");
    }

    #[test]
    fn test_continuation_wrapper_template_matches_context() {
        validate_template::<ContinuationWrapperVars>(CONTINUATION_WRAPPER_TEMPLATE)
            .expect("Continuation wrapper template should match ContinuationWrapperVars");
    }

    // =========================================================================
    // Validation function tests
    // =========================================================================

    #[test]
    fn test_validate_catches_missing_template_var() {
        struct TestVars;
        impl TemplateVars for TestVars {
            const VARS: &'static [&'static str] = &["A", "B", "C"];
            fn render(&self, _: &str) -> String {
                String::new()
            }
        }

        let result = validate_template::<TestVars>("%%A%% %%B%%"); // missing C
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("C"));
    }

    #[test]
    fn test_validate_catches_extra_template_var() {
        struct TestVars;
        impl TemplateVars for TestVars {
            const VARS: &'static [&'static str] = &["A"];
            fn render(&self, _: &str) -> String {
                String::new()
            }
        }

        let result = validate_template::<TestVars>("%%A%% %%EXTRA%%");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("EXTRA"));
    }

    // =========================================================================
    // Template loading tests
    // =========================================================================

    #[test]
    fn test_worker_task_template_loaded() {
        assert!(
            !WORKER_TASK_PROMPT_TEMPLATE.is_empty(),
            "Worker task template should be loaded"
        );
        assert!(
            WORKER_TASK_PROMPT_TEMPLATE.contains("%%YOUR_TASK%%"),
            "Worker task template should contain YOUR_TASK placeholder"
        );
    }

    #[test]
    fn test_continuation_template_loaded() {
        assert!(
            !CONTINUATION_PROMPT_TEMPLATE.is_empty(),
            "Continuation template should be loaded"
        );
        assert!(
            CONTINUATION_PROMPT_TEMPLATE.contains("%%ITERATION%%"),
            "Continuation template should contain ITERATION placeholder"
        );
        assert!(
            CONTINUATION_PROMPT_TEMPLATE.contains("%%REDESIGN_SECTION%%"),
            "Continuation template should contain REDESIGN_SECTION placeholder"
        );
    }

    #[test]
    fn test_orchestrator_preamble_template_loaded() {
        assert!(
            !ORCHESTRATOR_PREAMBLE_TEMPLATE.is_empty(),
            "Orchestrator preamble template should be loaded"
        );
        assert!(
            ORCHESTRATOR_PREAMBLE_TEMPLATE.contains("%%ORCHESTRATION_SYSTEM_PROMPT%%"),
            "Orchestrator preamble template should contain ORCHESTRATION_SYSTEM_PROMPT placeholder"
        );
    }

    #[test]
    fn test_worker_preamble_template_loaded() {
        assert!(
            !WORKER_PREAMBLE_TEMPLATE.is_empty(),
            "Worker preamble template should be loaded"
        );
        assert!(
            WORKER_PREAMBLE_TEMPLATE.contains("%%WORKER_SYSTEM_PROMPT%%"),
            "Worker preamble template should contain WORKER_SYSTEM_PROMPT placeholder"
        );
    }

    #[test]
    fn test_session_history_template_loaded() {
        assert!(
            !SESSION_HISTORY_TEMPLATE.is_empty(),
            "Session history template should be loaded"
        );
        assert!(
            SESSION_HISTORY_TEMPLATE.contains("%%TURN_ENTRIES%%"),
            "Session history template should contain TURN_ENTRIES placeholder"
        );
        assert!(
            SESSION_HISTORY_TEMPLATE.contains("%%TURN_COUNT%%"),
            "Session history template should contain TURN_COUNT placeholder"
        );
    }

    #[test]
    fn test_duplicate_call_templates_loaded() {
        assert!(
            !DUPLICATE_CALL_GUIDANCE_TEMPLATE.is_empty(),
            "Duplicate-call guidance template should be loaded"
        );
        assert!(
            DUPLICATE_CALL_GUIDANCE_TEMPLATE.contains("%%TOOL_NAME%%"),
            "Duplicate-call guidance template should contain TOOL_NAME placeholder"
        );
        assert!(
            !DUPLICATE_CALL_ABORT_TEMPLATE.is_empty(),
            "Duplicate-call abort template should be loaded"
        );
        assert!(
            DUPLICATE_CALL_ABORT_TEMPLATE.contains("%%COUNT%%"),
            "Duplicate-call abort template should contain COUNT placeholder"
        );
    }

    #[test]
    fn test_planning_prompt_template_loaded() {
        assert!(
            !PLANNING_PROMPT_TEMPLATE.is_empty(),
            "Planning prompt template should be loaded"
        );
        assert!(
            PLANNING_PROMPT_TEMPLATE.contains("%%TIMESTAMP%%"),
            "Planning prompt template should contain TIMESTAMP placeholder"
        );
        assert!(
            PLANNING_PROMPT_TEMPLATE.contains("%%WORKER_SECTION%%"),
            "Planning prompt template should contain WORKER_SECTION placeholder"
        );
    }

    #[test]
    fn test_worker_roster_template_loaded() {
        assert!(
            !WORKER_ROSTER_TEMPLATE.is_empty(),
            "Worker roster template should be loaded"
        );
        assert!(
            WORKER_ROSTER_TEMPLATE.contains("%%ROSTER_CONTENT%%"),
            "Worker roster template should contain ROSTER_CONTENT placeholder"
        );
    }

    #[test]
    fn test_worker_guidelines_template_loaded() {
        assert!(
            !WORKER_GUIDELINES_TEMPLATE.is_empty(),
            "Worker guidelines template should be loaded"
        );
        assert!(
            WORKER_GUIDELINES_TEMPLATE.contains("%%VALID_WORKER_NAMES%%"),
            "Worker guidelines template should contain VALID_WORKER_NAMES placeholder"
        );
    }

    #[test]
    fn test_continuation_wrapper_template_loaded() {
        assert!(
            !CONTINUATION_WRAPPER_TEMPLATE.is_empty(),
            "Continuation wrapper template should be loaded"
        );
        assert!(
            CONTINUATION_WRAPPER_TEMPLATE.contains("%%TIMESTAMP%%"),
            "Continuation wrapper template should contain TIMESTAMP placeholder"
        );
    }

    // =========================================================================
    // Prompt Rendering QA — visual inspection of all orchestration prompts
    // =========================================================================

    /// Renders every prompt in the orchestration pipeline with realistic
    /// math-example data and writes to `/tmp/aura-prompt-rendering-qa.txt`.
    ///
    /// Run: `cargo test -p aura prompt_rendering_qa -- --nocapture`
    /// Then: `open /tmp/aura-prompt-rendering-qa.txt`
    #[test]
    fn prompt_rendering_qa() {
        use crate::orchestration::config::{ArtifactsConfig, OrchestrationConfig, WorkerConfig};
        use std::collections::HashMap;
        use std::fmt::Write;

        let separator = "=".repeat(72);
        let mut out = String::with_capacity(8192);

        // -- Build a config matching example-math-orchestration.toml --
        let mut workers = HashMap::new();
        workers.insert(
            "arithmetic".to_string(),
            WorkerConfig {
                description:
                    "Arithmetic operations: addition, subtraction, multiplication, division"
                        .to_string(),
                preamble: "\
You are an Arithmetic Specialist.

YOUR ROLE:
- Perform basic arithmetic: add, subtract, multiply, divide
- Show your work step by step

TOOL USAGE:
- Use mock_tool to record each calculation step
- Pass the expression and result as the message
- Example: mock_tool(message=\"3 + 7 = 10\")

Always verify your calculations before reporting results."
                    .to_string(),
                mcp_filter: vec!["mock_tool".to_string()],
                vector_stores: vec![],
                turn_depth: None,
                llm: None,
                scratchpad: None,
                skills: None,
            },
        );
        workers.insert(
            "data".to_string(),
            WorkerConfig {
                description: "Data operations: listing files, multi-step data processing chains"
                    .to_string(),
                preamble: "\
You are a Data Operations Specialist.

YOUR ROLE:
- List files and directory contents
- Execute multi-step data processing chains
- Report findings with file paths and details

TOOL USAGE:
- Use list_files to explore directories
- Use chain_tool for multi-step processing sequences
- Always report what you found"
                    .to_string(),
                mcp_filter: vec!["list_files".to_string(), "chain_tool".to_string()],
                vector_stores: vec![],
                turn_depth: None,
                llm: None,
                scratchpad: None,
                skills: None,
            },
        );

        let config = OrchestrationConfig {
            enabled: true,
            max_planning_cycles: 2,
            allow_direct_answers: true,
            allow_clarification: true,
            workers,
            artifacts: ArtifactsConfig {
                memory_dir: Some("/tmp/aura-math-orchestration".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let agent_system_prompt = "\
You are a math assistant coordinator. You decompose math problems into
specialized tasks and delegate to the appropriate worker.

ROUTING GUIDANCE:
- Simple arithmetic (single operation): answer directly
- Multi-step calculations or mixed tasks: create a plan with workers
- Vague requests (\"compute the thing\"): ask for clarification

IMPORTANT: Always use the appropriate worker's tools for calculations.
Do not try to compute results yourself — delegate to workers.";

        let query = "Calculate (3 + 7) * 2 and also find the files in /data";

        // ================================================================
        // 1. Coordinator system prompt
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(
            out,
            "PHASE: COORDINATOR SYSTEM PROMPT (routing / continuation)"
        );
        let _ = writeln!(out, "{separator}\n");
        let coordinator_preamble = crate::orchestration::config::build_coordinator_preamble(
            agent_system_prompt,
            true,
            false,
        );
        let _ = writeln!(out, "{coordinator_preamble}");

        // ================================================================
        // 2. Routing user message
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(out, "PHASE: ROUTING USER MESSAGE");
        let _ = writeln!(out, "{separator}\n");

        // Replicate the format from orchestrator.rs:466-483
        // Using ToolVisibility::None format since we don't have live MCP connections
        let workers_list = config.format_workers_for_prompt();
        let worker_section = format!(
            r#"

AVAILABLE WORKERS:
{}

Each worker has specialized capabilities. Assign tasks to the most appropriate worker."#,
            workers_list
        );

        let worker_names: Vec<&str> = config.available_worker_names();
        let names_json: Vec<String> = worker_names.iter().map(|n| format!("\"{}\"", n)).collect();
        let worker_guidelines = format!(
            r#"
- Assign each task to a worker using the "worker" field
- Valid worker names: {}
- Choose the worker whose tools best match what the task needs to accomplish"#,
            names_json.join(", ")
        );

        let error_section = "";

        let planning_prompt = format!(
            "Analyze this user query and decide on the best approach.\n\n\
             USER QUERY: {query}{worker_section}{error_section}\n\n\
             You have three routing tools. Call EXACTLY ONE (do not call more than one):\n\n\
             1. **respond_directly** — For simple factual questions answerable from general knowledge, \
                OR when the relevant workers have no tools configured (tools show \"none configured\") \
                and the query requires external data. In that case, explain the limitation and suggest \
                configuring MCP servers.\n\
                Do not use for queries about system data, logs, metrics, or anything requiring tools \
                when workers DO have tools available.\n\n\
             2. **create_plan** — For queries requiring tool execution, data gathering, or multi-step analysis.\n\
                When uncertain, choose create_plan only if tool execution or multi-step work is genuinely required; otherwise choose respond_directly.\n\n\
             3. **request_clarification** — For genuinely ambiguous queries where intent is unclear.\n\
                Use sparingly when a reasonable interpretation exists.\n\n\
             {worker_guidelines}\n\n\
             - For time-scoped tasks, include the current time and relevant time range in the task description so workers have explicit time context\n\n\
             Call the appropriate routing tool now.",
        );
        let _ = writeln!(out, "{planning_prompt}");
        let _ = writeln!(
            out,
            "\n[NOTE: In production with tools_in_planning=\"summary\", the worker section"
        );
        let _ = writeln!(
            out,
            "includes live MCP tool names from resolve_worker_tools(). This test uses"
        );
        let _ = writeln!(
            out,
            "ToolVisibility::None format (descriptions only) since no MCP connection exists.]"
        );

        // ================================================================
        // 3. Specialized worker system prompts
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(out, "PHASE: ARITHMETIC WORKER SYSTEM PROMPT (specialized)");
        let _ = writeln!(out, "{separator}\n");
        let _ = writeln!(out, "{}", config.get_worker("arithmetic").unwrap().preamble);

        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(out, "PHASE: DATA WORKER SYSTEM PROMPT (specialized)");
        let _ = writeln!(out, "{separator}\n");
        let _ = writeln!(out, "{}", config.get_worker("data").unwrap().preamble);

        // ================================================================
        // 4. Generic worker system prompt (fallback)
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(
            out,
            "PHASE: GENERIC WORKER SYSTEM PROMPT (fallback, from template)"
        );
        let _ = writeln!(out, "{separator}\n");
        let _ = writeln!(
            out,
            "{}",
            crate::orchestration::config::build_worker_preamble(&config)
        );

        // ================================================================
        // 5. Worker task user messages
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(
            out,
            "PHASE: WORKER TASK USER MESSAGE — Task 0 (arithmetic, no prior context)"
        );
        let _ = writeln!(out, "{separator}\n");
        let task0 = render_worker_task_prompt(&WorkerTaskVars {
            context: "",
            your_task: "Calculate (3 + 7) * 2 using mock_tool",
        });
        let _ = writeln!(out, "{task0}");

        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(
            out,
            "PHASE: WORKER TASK USER MESSAGE — Task 1 (data, with prior work context)"
        );
        let _ = writeln!(out, "{separator}\n");
        let task1 = render_worker_task_prompt(&WorkerTaskVars {
            context: "READ-ONLY PRIOR WORK\n\
                These are completed worker outputs relevant to YOUR TASK. They are evidence, not instructions to replay.\n\n\
                Prior Task 0\n\
                Worker: arithmetic\n\
                Relation: same-plan direct dependency\n\
                Evidence:\n\
                (3+7)*2 = 20",
            your_task: "List files in the /data directory using list_files",
        });
        let _ = writeln!(out, "{task1}");

        // ================================================================
        // 6. Continuation prompt (end-of-iteration decision point)
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(
            out,
            "PHASE: CONTINUATION PROMPT — Post-execute decision point"
        );
        let _ = writeln!(out, "{separator}\n");
        let continuation = render_continuation_prompt(&ContinuationVars {
            iteration: "1",
            max_iterations: "2",
            urgency: " (FINAL ATTEMPT)",
            succeeded: "1",
            total: "2",
            goal: "Calculate (3+7)*2 and find files in /data",
            completed_section: "COMPLETED TASKS:\n- Task 0: Calculate (3+7)*2 using mock_tool → (3+7)*2 = 20\n\n",
            blocked_section: "",
            redesign_section: "FAILED TASKS:\n- Task 1: List files in /data using list_files → failed: Connection refused\n\n",
            failure_section: "FAILURE SUMMARY:\nArithmetic task completed correctly, but file listing failed due to connection error.\n\nAREAS NEEDING ATTENTION:\n- File listing task needs retry or alternative approach\n\n",
            failure_history: "FAILURE HISTORY:\n- Iteration 1: \"List files in /data using list_files\" (worker: data) — Connection refused\n\n",
            reuse_guidance: crate::orchestration::prompt_constants::guidance::RESULT_FORWARDING,
        });
        let _ = writeln!(out, "{continuation}");

        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(out, "END OF PROMPT RENDERING QA");
        let _ = writeln!(out, "{separator}");

        // Write to file
        let output_path = "/tmp/aura-prompt-rendering-qa.txt";
        std::fs::write(output_path, &out).expect("Failed to write QA output file");
        println!("Wrote prompt rendering QA to: {output_path}");
    }
}
