use crate::ConfigError;
use regex::Regex;
use std::env;

/// Resolve environment variables in the format {{ env.VAR_NAME }}
/// or with defaults {{ env.VAR_NAME | default: 'value' }}
pub fn resolve_env_vars(content: &str) -> Result<String, ConfigError> {
    // First, strip comments by parsing and re-serializing TOML
    // This ensures commented-out env vars don't cause errors
    let toml_value: toml::Value = toml::from_str(content).map_err(ConfigError::TomlParse)?;

    let content_without_comments = toml::to_string(&toml_value)
        .map_err(|e| ConfigError::EnvVar(format!("TOML serialize error: {e}")))?;

    // Pattern with optional default value: {{ env.VAR | default: 'value' }} or {{ env.VAR }}
    let re_with_default =
        Regex::new(r"\{\{\s*env\.([A-Z_][A-Z0-9_]*)\s*\|\s*default:\s*'([^']*)'\s*\}\}")
            .map_err(|e| ConfigError::EnvVar(format!("Invalid regex: {e}")))?;
    let re_simple = Regex::new(r"\{\{\s*env\.([A-Z_][A-Z0-9_]*)\s*\}\}")
        .map_err(|e| ConfigError::EnvVar(format!("Invalid regex: {e}")))?;

    let mut result = content_without_comments.to_string();

    // First: replace all env vars with defaults (they never fail)
    for cap in re_with_default.captures_iter(&content_without_comments) {
        let var_name = &cap[1];
        let default_value = &cap[2];
        let replacement = env::var(var_name).unwrap_or_else(|_| default_value.to_string());
        result = result.replace(&cap[0], &replacement);
    }

    // Second: collect missing variables without defaults
    let mut missing_vars = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Re-parse result to check remaining simple env vars
    for cap in re_simple.captures_iter(&result.clone()) {
        let var_name = &cap[1];
        if env::var(var_name).is_err() && seen.insert(var_name.to_string()) {
            missing_vars.push(var_name.to_string());
        }
    }

    // If any variables are missing, report them all at once
    if !missing_vars.is_empty() {
        let vars_list = missing_vars.join(", ");
        let export_cmds = missing_vars
            .iter()
            .map(|v| format!("export {v}=your_value"))
            .collect::<Vec<_>>()
            .join("\n");

        return Err(ConfigError::EnvVar(format!(
            "Missing environment variable(s): {vars_list}\n\n\
            To fix this:\n\
            1. Copy .env.example to .env and fill in your API keys\n\
            2. Or export the variables in your shell:\n{export_cmds}"
        )));
    }

    // Third: replace remaining simple env vars (we know they exist)
    for cap in re_simple.captures_iter(&result.clone()) {
        let var_name = &cap[1];
        let replacement = env::var(var_name).unwrap();
        result = result.replace(&cap[0], &replacement);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commented_env_vars_are_ignored() {
        let toml_with_commented_env_var = r#"
# This comment has an undefined env var that should be ignored
# test_key = "{{ env.NONEXISTENT_VAR_12345 }}"

[llm]
provider = "openai"
api_key = "real_key"
model = "gpt-4o"
"#;

        // Should NOT error about NONEXISTENT_VAR_12345
        let result = resolve_env_vars(toml_with_commented_env_var);
        assert!(result.is_ok(), "Should not fail on commented env vars");

        let resolved = result.unwrap();
        assert!(
            !resolved.contains("NONEXISTENT_VAR_12345"),
            "Commented env var should be stripped"
        );
    }

    #[test]
    fn test_inline_comments_are_stripped() {
        let toml_with_inline_comment = r#"
[llm]
provider = "openai"  # This is a comment with {{ env.UNUSED_VAR }}
api_key = "test"
"#;

        let result = resolve_env_vars(toml_with_inline_comment);
        assert!(
            result.is_ok(),
            "Should not fail on inline comment with env var"
        );
    }

    #[test]
    fn test_default_value_when_env_missing() {
        let toml_with_default = r#"
[server]
host = "{{ env.TEST_HOST_NONEXISTENT | default: 'localhost' }}"
"#;

        let result = resolve_env_vars(toml_with_default);
        assert!(result.is_ok(), "Should use default when env var missing");

        let resolved = result.unwrap();
        assert!(
            resolved.contains("localhost"),
            "Should contain default value 'localhost'"
        );
    }

    #[test]
    fn test_default_value_when_env_set() {
        let _env_lock = crate::test_env_lock::lock();
        unsafe {
            env::set_var("TEST_HOST_EXISTS", "myhost");
        }

        let toml_with_default = r#"
[server]
host = "{{ env.TEST_HOST_EXISTS | default: 'localhost' }}"
"#;

        let result = resolve_env_vars(toml_with_default);
        assert!(result.is_ok(), "Should use env var when set");

        let resolved = result.unwrap();
        assert!(
            resolved.contains("myhost"),
            "Should contain env var value 'myhost'"
        );

        unsafe {
            env::remove_var("TEST_HOST_EXISTS");
        }
    }
}
