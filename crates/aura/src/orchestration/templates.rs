//! Prompt template rendering with type-safe validation.
//!
//! This module provides a simple template system using `%%VAR%%` placeholders
//! that avoids conflicts with JSON `{{`/`}}` literals and Rust format strings.
//!
//! # Type Safety
//!
//! Each template has an associated context type implementing `TemplateContext`.
//! Tests validate bi-directionally that:
//! - All template placeholders are provided by the context
//! - All context fields are used in the template
//!
//! # Example
//!
//! ```ignore
//! let rendered = render_worker_task_prompt(&WorkerTaskVars {
//!     orchestration_goal: "Analyze logs",
//!     context: "",
//!     your_task: "List all error entries",
//! });
//! ```

#[cfg(test)]
use std::collections::HashSet;

// Template constants loaded at compile time
pub const WORKER_TASK_PROMPT_TEMPLATE: &str = include_str!("../prompts/worker_task_prompt.md");
pub const SYNTHESIS_PROMPT_TEMPLATE: &str = include_str!("../prompts/synthesis_prompt.md");
pub const EVALUATION_PROMPT_TEMPLATE: &str = include_str!("../prompts/evaluation_prompt.md");
pub const PHASE_CONTINUATION_PROMPT_TEMPLATE: &str =
    include_str!("../prompts/phase_continuation.md");

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
    pub orchestration_goal: &'a str,
    pub context: &'a str,
    pub your_task: &'a str,
}

impl TemplateVars for WorkerTaskVars<'_> {
    const VARS: &'static [&'static str] = &["ORCHESTRATION_GOAL", "CONTEXT", "YOUR_TASK"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%ORCHESTRATION_GOAL%%", self.orchestration_goal)
            .replace("%%CONTEXT%%", self.context)
            .replace("%%YOUR_TASK%%", self.your_task)
    }
}

/// Variables for the synthesis prompt.
#[derive(Debug, Clone)]
pub struct SynthesisVars<'a> {
    pub goal: &'a str,
    pub query: &'a str,
    pub results: &'a str,
}

impl TemplateVars for SynthesisVars<'_> {
    const VARS: &'static [&'static str] = &["GOAL", "QUERY", "RESULTS"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%GOAL%%", self.goal)
            .replace("%%QUERY%%", self.query)
            .replace("%%RESULTS%%", self.results)
    }
}

/// Variables for the evaluation prompt.
#[derive(Debug, Clone)]
pub struct EvaluationVars<'a> {
    pub query: &'a str,
    pub goal: &'a str,
    pub workers_context: &'a str,
    pub result: &'a str,
}

impl TemplateVars for EvaluationVars<'_> {
    const VARS: &'static [&'static str] = &["QUERY", "GOAL", "WORKERS_CONTEXT", "RESULT"];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%QUERY%%", self.query)
            .replace("%%GOAL%%", self.goal)
            .replace("%%WORKERS_CONTEXT%%", self.workers_context)
            .replace("%%RESULT%%", self.result)
    }
}

/// Variables for the phase continuation prompt.
#[derive(Debug, Clone)]
pub struct PhaseContinuationVars<'a> {
    pub completed_phase_label: &'a str,
    pub completed_phase_id: &'a str,
    pub goal: &'a str,
    pub completed_phase_results: &'a str,
    pub remaining_phases: &'a str,
}

impl TemplateVars for PhaseContinuationVars<'_> {
    const VARS: &'static [&'static str] = &[
        "COMPLETED_PHASE_LABEL",
        "COMPLETED_PHASE_ID",
        "GOAL",
        "COMPLETED_PHASE_RESULTS",
        "REMAINING_PHASES",
    ];

    fn render(&self, template: &str) -> String {
        template
            .replace("%%COMPLETED_PHASE_LABEL%%", self.completed_phase_label)
            .replace("%%COMPLETED_PHASE_ID%%", self.completed_phase_id)
            .replace("%%GOAL%%", self.goal)
            .replace("%%COMPLETED_PHASE_RESULTS%%", self.completed_phase_results)
            .replace("%%REMAINING_PHASES%%", self.remaining_phases)
    }
}

/// Render the phase continuation prompt with the given variables.
pub fn render_phase_continuation_prompt(vars: &PhaseContinuationVars<'_>) -> String {
    vars.render(PHASE_CONTINUATION_PROMPT_TEMPLATE)
}

/// Render the worker task prompt with the given variables.
pub fn render_worker_task_prompt(vars: &WorkerTaskVars<'_>) -> String {
    vars.render(WORKER_TASK_PROMPT_TEMPLATE)
}

/// Render the synthesis prompt with the given variables.
pub fn render_synthesis_prompt(vars: &SynthesisVars<'_>) -> String {
    vars.render(SYNTHESIS_PROMPT_TEMPLATE)
}

/// Render the evaluation prompt with the given variables.
pub fn render_evaluation_prompt(vars: &EvaluationVars<'_>) -> String {
    vars.render(EVALUATION_PROMPT_TEMPLATE)
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
        let vars = SynthesisVars {
            goal: "test goal",
            query: "test query",
            results: "test results",
        };
        let rendered = vars.render("Goal: %%GOAL%%, Query: %%QUERY%%");
        assert_eq!(rendered, "Goal: test goal, Query: test query");
    }

    #[test]
    fn test_render_empty_value() {
        let vars = SynthesisVars {
            goal: "test",
            query: "test",
            results: "",
        };
        let rendered = vars.render("Start%%RESULTS%%End");
        assert_eq!(rendered, "StartEnd");
    }

    #[test]
    fn test_render_preserves_json_braces() {
        let vars = SynthesisVars {
            goal: "test",
            query: "test",
            results: "test",
        };
        let template = r#"{"goal": "%%GOAL%%", "nested": {"key": "value"}}"#;
        let rendered = vars.render(template);
        assert_eq!(rendered, r#"{"goal": "test", "nested": {"key": "value"}}"#);
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
    fn test_synthesis_template_matches_context() {
        validate_template::<SynthesisVars>(SYNTHESIS_PROMPT_TEMPLATE)
            .expect("Synthesis template should match SynthesisVars");
    }

    #[test]
    fn test_evaluation_template_matches_context() {
        validate_template::<EvaluationVars>(EVALUATION_PROMPT_TEMPLATE)
            .expect("Evaluation template should match EvaluationVars");
    }

    #[test]
    fn test_phase_continuation_template_matches_context() {
        validate_template::<PhaseContinuationVars>(PHASE_CONTINUATION_PROMPT_TEMPLATE)
            .expect("Phase continuation template should match PhaseContinuationVars");
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
    fn test_synthesis_template_loaded() {
        assert!(
            !SYNTHESIS_PROMPT_TEMPLATE.is_empty(),
            "Synthesis template should be loaded"
        );
        assert!(
            SYNTHESIS_PROMPT_TEMPLATE.contains("%%RESULTS%%"),
            "Synthesis template should contain RESULTS placeholder"
        );
    }

    #[test]
    fn test_evaluation_template_loaded() {
        assert!(
            !EVALUATION_PROMPT_TEMPLATE.is_empty(),
            "Evaluation template should be loaded"
        );
        assert!(
            EVALUATION_PROMPT_TEMPLATE.contains("%%RESULT%%"),
            "Evaluation template should contain RESULT placeholder"
        );
    }

    #[test]
    fn test_phase_continuation_template_loaded() {
        assert!(
            !PHASE_CONTINUATION_PROMPT_TEMPLATE.is_empty(),
            "Phase continuation template should be loaded"
        );
        assert!(
            PHASE_CONTINUATION_PROMPT_TEMPLATE.contains("%%COMPLETED_PHASE_LABEL%%"),
            "Phase continuation template should contain COMPLETED_PHASE_LABEL placeholder"
        );
        assert!(
            PHASE_CONTINUATION_PROMPT_TEMPLATE.contains("%%REMAINING_PHASES%%"),
            "Phase continuation template should contain REMAINING_PHASES placeholder"
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
            },
        );

        let config = OrchestrationConfig {
            enabled: true,
            max_planning_cycles: 2,
            quality_threshold: 0.7,
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
            "PHASE: COORDINATOR SYSTEM PROMPT (routing / synthesis / evaluation)"
        );
        let _ = writeln!(out, "{separator}\n");
        let coordinator_preamble = config.build_coordinator_preamble(agent_system_prompt, true);
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

        let reflection_section = "";
        let error_section = "";

        let planning_prompt = format!(
            "Analyze this user query and decide on the best approach.\n\n\
             USER QUERY: {query}{worker_section}{reflection_section}{error_section}\n\n\
             You have three routing tools. Call EXACTLY ONE (do not call more than one):\n\n\
             1. **respond_directly** — For simple factual questions answerable from general knowledge.\n\
                NEVER use for queries about system data, logs, metrics, or anything requiring tools.\n\n\
             2. **create_plan** — For queries requiring tool execution, data gathering, or multi-step analysis.\n\
                When uncertain between respond_directly and create_plan, always choose create_plan.\n\n\
             3. **request_clarification** — For genuinely ambiguous queries where intent is unclear.\n\
                Use sparingly — prefer create_plan when a reasonable interpretation exists.\n\n\
             {worker_guidelines}\n\n\
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
        let _ = writeln!(out, "{}", config.build_worker_preamble());

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
            orchestration_goal: "Calculate (3+7)*2 and find files in /data",
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
            orchestration_goal: "Calculate (3+7)*2 and find files in /data",
            context: "COMPLETED — Task 0 (arithmetic): (3+7)*2 = 20\n\n",
            your_task: "List files in the /data directory using list_files",
        });
        let _ = writeln!(out, "{task1}");

        // ================================================================
        // 6. Synthesis user message
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(out, "PHASE: SYNTHESIS USER MESSAGE");
        let _ = writeln!(out, "{separator}\n");
        let synthesis = render_synthesis_prompt(&SynthesisVars {
            goal: "Calculate (3+7)*2 and find files in /data",
            query,
            results: "Task 0 (arithmetic):\n  Result: (3+7)*2 = 20\n\nTask 1 (data):\n  Result: /data contains file1.txt, file2.txt",
        });
        let _ = writeln!(out, "{synthesis}");

        // ================================================================
        // 7. Evaluation user message
        // ================================================================
        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(out, "PHASE: EVALUATION USER MESSAGE");
        let _ = writeln!(out, "{separator}\n");
        let evaluation = render_evaluation_prompt(&EvaluationVars {
            query,
            goal: "Calculate (3+7)*2 and find files in /data",
            workers_context: "\nSYSTEM CONTEXT:\nConfigured workers:\n- arithmetic: Arithmetic operations: addition, subtraction, multiplication, division\n- data: Data operations: listing files, multi-step data processing chains\n\n",
            result: "(3+7)*2 equals 20. The /data directory contains file1.txt and file2.txt.",
        });
        let _ = writeln!(out, "{evaluation}");

        let _ = writeln!(out, "\n{separator}");
        let _ = writeln!(out, "END OF PROMPT RENDERING QA");
        let _ = writeln!(out, "{separator}");

        // Write to file
        let output_path = "/tmp/aura-prompt-rendering-qa.txt";
        std::fs::write(output_path, &out).expect("Failed to write QA output file");
        println!("Wrote prompt rendering QA to: {output_path}");
    }
}
