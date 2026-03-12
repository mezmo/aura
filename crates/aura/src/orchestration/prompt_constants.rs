//! Prompt section headers and field names as constants for type safety.
//!
//! This module centralizes all string constants used in orchestration prompts,
//! ensuring consistency across the codebase and enabling easy updates.
//!
//! # Research Inspirations
//!
//! Based on patterns from:
//! - LangChain's Write-Select-Compress-Isolate framework
//! - LlamaIndex's Context Store with typed state
//! - Anthropic's context engineering principles

/// Section headers for worker prompts.
pub mod sections {
    pub const ORCHESTRATION_GOAL: &str = "ORCHESTRATION GOAL";
    pub const PRIOR_WORK: &str = "COMPLETED";
    pub const YOUR_TASK: &str = "YOUR TASK";
}

/// JSON field names for plan parsing.
pub mod fields {
    pub const GOAL: &str = "goal";
    pub const TASKS: &str = "tasks";
    pub const ID: &str = "id";
    pub const DESCRIPTION: &str = "description";
    pub const DEPENDENCIES: &str = "dependencies";
    pub const RATIONALE: &str = "rationale";
    pub const WORKER: &str = "worker";
}

/// Context formatting constants.
pub mod context {
    pub const DEPENDENCY_SEPARATOR: &str = "\n\n---\n\n";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_section_headers_are_uppercase() {
        assert_eq!(sections::ORCHESTRATION_GOAL.chars().next().unwrap(), 'O');
        assert!(
            sections::PRIOR_WORK
                .chars()
                .all(|c| c.is_uppercase() || c.is_whitespace())
        );
    }

    #[test]
    fn test_field_names_are_lowercase() {
        assert!(fields::GOAL.chars().all(|c| c.is_lowercase()));
        assert!(fields::RATIONALE.chars().all(|c| c.is_lowercase()));
    }
}
