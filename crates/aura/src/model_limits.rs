//! Model context limits for LLM providers.
//!
//! Provides static mapping of model names to their context window limits.
//! Used for calculating context usage percentage in the UI.
//!
//! Reference: Internal context window quick reference (2026-01)

use std::collections::HashMap;
use std::sync::LazyLock;

/// Static map of model names to their context window limits (in tokens).
///
/// This map contains known context limits for various LLM models.
/// The `get_context_limit` function also handles prefix matching for
/// model variants (e.g., "gpt-4o-2024-08-06" matches "gpt-4o").
pub static MODEL_CONTEXT_LIMITS: LazyLock<HashMap<&'static str, u32>> = LazyLock::new(|| {
    let mut m = HashMap::new();

    // OpenAI GPT-5.x (API) - 400K context
    m.insert("gpt-5.2", 400_000);
    m.insert("gpt-5.2-pro", 400_000);
    m.insert("gpt-5.1", 400_000);
    m.insert("gpt-5", 400_000);
    m.insert("gpt-5-mini", 400_000);
    m.insert("gpt-5-nano", 400_000);

    // OpenAI GPT-5.x (Chat) - 128K context
    m.insert("gpt-5.2-chat-latest", 128_000);
    m.insert("gpt-5.1-chat-latest", 128_000);
    m.insert("gpt-5-chat-latest", 128_000);

    // OpenAI GPT-4.1 - 1M context
    m.insert("gpt-4.1", 1_000_000);
    m.insert("gpt-4.1-mini", 1_000_000);
    m.insert("gpt-4.1-nano", 1_000_000);

    // OpenAI GPT-4.x - 128K context
    m.insert("gpt-4o", 128_000);
    m.insert("gpt-4o-mini", 128_000);
    m.insert("gpt-4-turbo", 128_000);
    m.insert("gpt-4", 8_192);
    m.insert("gpt-4-32k", 32_768);
    m.insert("gpt-3.5-turbo", 16_385);
    m.insert("gpt-3.5-turbo-16k", 16_385);

    // OpenAI o-series (reasoning models)
    m.insert("o3", 200_000);
    m.insert("o3-pro", 200_000);
    m.insert("o3-mini", 128_000);
    m.insert("o4-mini", 200_000);
    m.insert("o1", 128_000);
    m.insert("o1-mini", 128_000);
    m.insert("o1-preview", 128_000);

    // Anthropic Claude 4.5
    m.insert("claude-opus-4-5-20251101", 200_000);
    m.insert("claude-sonnet-4-5-20250929", 200_000);
    m.insert("claude-haiku-4-5-20251001", 200_000);

    // Anthropic Claude 4
    m.insert("claude-opus-4-1-20250805", 200_000);
    m.insert("claude-opus-4-20250522", 200_000);
    m.insert("claude-sonnet-4-20250514", 200_000);

    // Anthropic Claude 3.x
    m.insert("claude-3-7-sonnet-20250219", 200_000);
    m.insert("claude-3-5-sonnet-20241022", 200_000);
    m.insert("claude-3-5-haiku-20241022", 200_000);
    m.insert("claude-3-opus-20240229", 200_000);
    // Legacy short names (for prefix matching)
    m.insert("claude-3-opus", 200_000);
    m.insert("claude-3-sonnet", 200_000);
    m.insert("claude-3-haiku", 200_000);
    m.insert("claude-3.5-sonnet", 200_000);
    m.insert("claude-3.5-haiku", 200_000);
    m.insert("claude-3-5-sonnet", 200_000);
    m.insert("claude-3-5-haiku", 200_000);

    // AWS Bedrock model IDs (standard)
    m.insert("anthropic.claude-3-opus", 200_000);
    m.insert("anthropic.claude-3-sonnet", 200_000);
    m.insert("anthropic.claude-3-haiku", 200_000);
    m.insert("anthropic.claude-3-5-sonnet", 200_000);
    m.insert("anthropic.claude-3-5-haiku", 200_000);

    // AWS Bedrock cross-region inference IDs (us.*, eu.*)
    m.insert("us.anthropic.claude-3-opus", 200_000);
    m.insert("us.anthropic.claude-3-sonnet", 200_000);
    m.insert("us.anthropic.claude-3-haiku", 200_000);
    m.insert("us.anthropic.claude-3-5-sonnet", 200_000);
    m.insert("us.anthropic.claude-3-5-haiku", 200_000);
    m.insert("eu.anthropic.claude-3-sonnet", 200_000);
    m.insert("eu.anthropic.claude-3-haiku", 200_000);

    // Google Gemini 3 (preview)
    m.insert("gemini-3-pro-preview", 1_000_000);
    m.insert("gemini-3-flash-preview", 1_000_000);

    // Google Gemini 2.x
    m.insert("gemini-2.5-pro", 1_000_000);
    m.insert("gemini-2.5-flash", 1_000_000);
    m.insert("gemini-2.0-flash", 1_000_000);
    m.insert("gemini-2.0-pro-exp", 2_000_000);

    // Google Gemini 1.x
    m.insert("gemini-1.5-pro", 2_000_000);
    m.insert("gemini-1.5-flash", 1_000_000);
    m.insert("gemini-1.0-pro", 32_000);

    // Ollama / Local - Qwen3
    m.insert("qwen3:0.6b", 32_000);
    m.insert("qwen3:1.7b", 32_000);
    m.insert("qwen3:4b", 40_000);
    m.insert("qwen3:8b", 40_000);
    m.insert("qwen3:14b", 40_000);
    m.insert("qwen3:32b", 40_000);
    m.insert("qwen3:30b", 256_000);
    m.insert("qwen3:235b", 256_000);
    m.insert("qwen3-coder", 256_000);
    m.insert("qwen3-vl", 256_000);
    m.insert("qwq", 128_000);

    // Ollama / Local - Qwen2.5
    m.insert("qwen2.5", 128_000);
    m.insert("qwen2.5:32b", 128_000);
    m.insert("qwen2.5:72b", 128_000);
    m.insert("qwen2.5-1m", 1_000_000);
    m.insert("qwen2.5-coder", 128_000);

    // Ollama / Local - DeepSeek
    m.insert("deepseek-r1", 128_000);
    m.insert("deepseek-r1:8b", 128_000);
    m.insert("deepseek-r1:32b", 128_000);
    m.insert("deepseek-r1:70b", 128_000);
    m.insert("deepseek-v3", 128_000);
    m.insert("deepseek-v3.1", 128_000);
    m.insert("deepseek-coder", 128_000);

    // Ollama / Local - Llama 4
    m.insert("llama4:scout", 10_000_000);
    m.insert("llama4:maverick", 1_000_000);

    // Ollama / Local - Llama 3.x
    m.insert("llama3.3:70b", 128_000);
    m.insert("llama3.2", 128_000);
    m.insert("llama3.1", 128_000);
    m.insert("llama3", 8_000);

    // Ollama / Local - Mistral
    m.insert("mistral-large", 128_000);
    m.insert("mistral-small", 128_000);
    m.insert("mistral-nemo", 128_000);
    m.insert("mixtral:8x22b", 64_000);
    m.insert("mixtral:8x7b", 32_000);
    m.insert("codestral", 256_000);

    // Ollama / Local - Google Gemma
    m.insert("gemma3:1b", 32_000);
    m.insert("gemma3", 128_000);
    m.insert("gemma2", 8_000);

    // Ollama / Local - Microsoft Phi
    m.insert("phi4", 16_000);
    m.insert("phi4-mini", 128_000);
    m.insert("phi4-reasoning", 32_000);
    m.insert("phi3", 128_000);

    m
});

/// Get the context limit for a model.
///
/// Performs exact match first, then falls back to prefix matching.
/// This handles model variants like "gpt-4o-2024-08-06" matching "gpt-4o".
/// Prefix matching prefers longer matches (e.g., "gpt-4o" over "gpt-4").
///
/// # Arguments
/// * `model` - The model name or identifier
///
/// # Returns
/// * `Some(limit)` - The context limit in tokens if found
/// * `None` - If the model is not recognized
///
/// # Examples
/// ```
/// use aura::model_limits::get_context_limit;
///
/// assert_eq!(get_context_limit("gpt-4o"), Some(128_000));
/// assert_eq!(get_context_limit("gpt-4o-2024-08-06"), Some(128_000));
/// assert_eq!(get_context_limit("claude-3-opus-20240229"), Some(200_000));
/// assert_eq!(get_context_limit("openai/gpt-5.2"), Some(400_000));
/// assert_eq!(get_context_limit("unknown-model"), None);
/// ```
pub fn get_context_limit(model: &str) -> Option<u32> {
    // Strip provider prefix if present (e.g., "openai/gpt-4o" -> "gpt-4o")
    let model_name = model.split('/').next_back().unwrap_or(model);

    MODEL_CONTEXT_LIMITS.get(model_name).copied().or_else(|| {
        // Fall back to prefix matching (for model variants with date suffixes)
        // Use longest match to prefer "gpt-4o" over "gpt-4" for "gpt-4o-2024-08-06"
        MODEL_CONTEXT_LIMITS
            .iter()
            .filter(|(k, _)| model_name.starts_with(*k))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| *v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        assert_eq!(get_context_limit("gpt-4o"), Some(128_000));
        assert_eq!(get_context_limit("gpt-4"), Some(8_192));
        assert_eq!(get_context_limit("claude-3-opus"), Some(200_000));
    }

    #[test]
    fn test_prefix_match() {
        assert_eq!(get_context_limit("gpt-4o-2024-08-06"), Some(128_000));
        assert_eq!(get_context_limit("claude-3-opus-20240229"), Some(200_000));
        assert_eq!(get_context_limit("gpt-4-turbo-preview"), Some(128_000));
    }

    #[test]
    fn test_unknown_model() {
        assert_eq!(get_context_limit("unknown-model"), None);
        assert_eq!(get_context_limit(""), None);
    }

    #[test]
    fn test_reasoning_models() {
        assert_eq!(get_context_limit("o1"), Some(128_000));
        assert_eq!(get_context_limit("o3"), Some(200_000));
        assert_eq!(get_context_limit("o3-mini"), Some(128_000));
        assert_eq!(get_context_limit("o4-mini"), Some(200_000));
    }

    #[test]
    fn test_gemini_models() {
        assert_eq!(get_context_limit("gemini-1.5-pro"), Some(2_000_000));
        assert_eq!(get_context_limit("gemini-2.0-flash"), Some(1_000_000));
        assert_eq!(get_context_limit("gemini-2.5-pro"), Some(1_000_000));
        assert_eq!(get_context_limit("gemini-3-pro-preview"), Some(1_000_000));
    }

    #[test]
    fn test_gpt5_models() {
        assert_eq!(get_context_limit("gpt-5.2"), Some(400_000));
        assert_eq!(get_context_limit("gpt-5.1"), Some(400_000));
        assert_eq!(get_context_limit("gpt-5"), Some(400_000));
        assert_eq!(get_context_limit("gpt-5-mini"), Some(400_000));
    }

    #[test]
    fn test_gpt41_models() {
        assert_eq!(get_context_limit("gpt-4.1"), Some(1_000_000));
        assert_eq!(get_context_limit("gpt-4.1-mini"), Some(1_000_000));
    }

    #[test]
    fn test_claude4_models() {
        assert_eq!(get_context_limit("claude-opus-4-5-20251101"), Some(200_000));
        assert_eq!(get_context_limit("claude-sonnet-4-20250514"), Some(200_000));
    }

    #[test]
    fn test_ollama_models() {
        assert_eq!(get_context_limit("llama4:scout"), Some(10_000_000));
        assert_eq!(get_context_limit("llama3.1"), Some(128_000));
        assert_eq!(get_context_limit("qwen3:8b"), Some(40_000));
        assert_eq!(get_context_limit("deepseek-r1"), Some(128_000));
        assert_eq!(get_context_limit("mistral-large"), Some(128_000));
    }

    #[test]
    fn test_provider_prefixed_models() {
        // Models with provider prefix (e.g., from OpenRouter or gateway configs)
        assert_eq!(get_context_limit("openai/gpt-5.2"), Some(400_000));
        assert_eq!(get_context_limit("openai/gpt-4o"), Some(128_000));
        assert_eq!(get_context_limit("anthropic/claude-3-opus"), Some(200_000));
        assert_eq!(
            get_context_limit("google/gemini-2.0-flash"),
            Some(1_000_000)
        );
    }

    #[test]
    fn test_bedrock_cross_region_models() {
        // AWS Bedrock cross-region inference IDs
        assert_eq!(
            get_context_limit("us.anthropic.claude-3-5-sonnet-20241022-v2:0"),
            Some(200_000)
        );
        assert_eq!(
            get_context_limit("us.anthropic.claude-3-haiku-20240307-v1:0"),
            Some(200_000)
        );
        assert_eq!(
            get_context_limit("eu.anthropic.claude-3-sonnet-20240229-v1:0"),
            Some(200_000)
        );
    }

    #[test]
    fn test_edge_cases() {
        // Empty string
        assert_eq!(get_context_limit(""), None);

        // Multiple slashes (takes last segment)
        assert_eq!(get_context_limit("provider/sub/gpt-4o"), Some(128_000));

        // Trailing slash (empty last segment)
        assert_eq!(get_context_limit("gpt-4o/"), None);

        // Just a slash
        assert_eq!(get_context_limit("/"), None);

        // Model with numbers that could match multiple prefixes
        assert_eq!(get_context_limit("gpt-4-32k-0613"), Some(32_768));
    }
}
