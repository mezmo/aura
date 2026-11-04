//! Classifying provider errors by their text.
//!
//! The text is owned by the provider and by rig's error wrapping, so each
//! check matches the provider's own wording rather than parsing a body.

/// User-facing explanation for a provider account out of credit or quota.
/// Carries no provider detail, so it is safe to show on a server that hides
/// raw provider errors.
pub const QUOTA_EXHAUSTED_MESSAGE: &str = "The model provider rejected the request because the \
    account has no remaining credit or quota. Add credit to the provider account, or raise its \
    quota, and try again.";

/// Whether `error` says the provider account has run out of credit or quota.
///
/// No retry or replan can clear this, and every further call is refused the
/// same way, so callers stop instead of spending more calls. Matches:
///
/// - Anthropic: HTTP 400, `credit balance is too low`
/// - OpenAI: HTTP 429, `insufficient_quota` / `exceeded your current quota`.
///   The 429 is why this must be checked before any rate-limit rule.
/// - OpenRouter: HTTP 402, `Insufficient credits`
///
/// Deliberately not matched: a bare `billing`, `quota` or `payment required`,
/// which a worker's own tool error (a billing service, a storage quota, a
/// payments API answering 402) could contain. A 402 counts only in rig's
/// `Invalid status code` wording, which marks a provider response.
pub fn is_quota_exhausted(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("credit balance is too low")
        || lower.contains("insufficient_quota")
        || lower.contains("exceeded your current quota")
        || lower.contains("insufficient credits")
        || lower.contains("invalid status code 402")
        || lower.contains("invalid status code: 402")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_providers_quota_error_is_recognized() {
        let anthropic = r#"CompletionError: ProviderError: SSE Error: Invalid status code 400 Bad Request with message: {"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."},"request_id":"req_011CeNSF3my2nQECgwDCGSSN"}"#;
        let openai = r#"CompletionError: ProviderError: Invalid status code 429 Too Many Requests with message: {"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        let openrouter = r#"CompletionError: ProviderError: Invalid status code 402 Payment Required with message: {"error":{"message":"Insufficient credits","code":402}}"#;
        for error in [anthropic, openai, openrouter] {
            assert!(is_quota_exhausted(error), "{error}");
        }
    }

    #[test]
    fn rate_limits_and_unrelated_billing_text_are_not_quota_errors() {
        for error in [
            "Invalid status code 429 Too Many Requests with message: rate limit exceeded",
            "Invalid status code 400 Bad Request with message: prompt is too long",
            "tool billing_lookup failed: billing service unavailable",
            "disk quota exceeded writing scratchpad file",
            "tool charge_card failed: HTTP 402 Payment Required",
        ] {
            assert!(!is_quota_exhausted(error), "{error}");
        }
    }
}
