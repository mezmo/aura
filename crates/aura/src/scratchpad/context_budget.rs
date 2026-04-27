//! Context budget tracking for scratchpad tools.
//!
//! Tracks estimated token usage from scratchpad tool returns and provides
//! remaining-budget hints to prevent context window overflow.
//!
//! Uses real tokenizers for accurate token counting via the `TokenCounter`
//! trait. Currently all providers use `tiktoken-rs` — OpenAI models resolve
//! to their exact tokenizer, others default to `o200k_base`. Additional
//! provider-specific tokenizers can be added by implementing `TokenCounter`
//! and updating `token_counter_for_provider`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ============================================================================
// TokenCounter trait + implementations
// ============================================================================

/// Provider-aware token counter.
pub trait TokenCounter: Send + Sync {
    fn count_tokens(&self, text: &str) -> usize;
}

impl std::fmt::Debug for dyn TokenCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenCounter")
    }
}

/// Token counter using tiktoken-rs BPE tokenizers.
///
/// For OpenAI models, resolves the exact tokenizer (o200k_base for GPT-5/GPT-4o/o-series,
/// cl100k_base for GPT-4/3.5). For all other providers, falls back to o200k_base.
pub struct TiktokenCounter {
    bpe: tiktoken_rs::CoreBPE,
}

impl TiktokenCounter {
    /// Create a counter for a specific model.
    /// Falls back to `o200k_base` if the model isn't recognized.
    pub fn for_model(model: &str) -> Self {
        let bpe = tiktoken_rs::get_bpe_from_model(model)
            .unwrap_or_else(|_| tiktoken_rs::o200k_base().unwrap());
        Self { bpe }
    }

    /// Create a counter using `o200k_base` (conservative default).
    pub fn default_counter() -> Self {
        Self {
            bpe: tiktoken_rs::o200k_base().unwrap(),
        }
    }
}

impl TokenCounter for TiktokenCounter {
    fn count_tokens(&self, text: &str) -> usize {
        self.bpe.encode_with_special_tokens(text).len()
    }
}

/// Create a token counter for the given provider and model.
///
/// Currently uses tiktoken-rs for all providers. OpenAI models resolve to
/// their exact tokenizer; others default to `o200k_base`. To add a
/// provider-specific tokenizer, implement `TokenCounter` and add a match arm.
pub fn token_counter_for_provider(provider: &str, model: &str) -> Arc<dyn TokenCounter> {
    match provider {
        "openai" => Arc::new(TiktokenCounter::for_model(model)),
        // Future: add dedicated tokenizers for other providers here
        // "anthropic" => Arc::new(ClaudeCounter::new()),
        _ => Arc::new(TiktokenCounter::for_model(model)),
    }
}

// ============================================================================
// ContextBudget
// ============================================================================

/// Tracks estimated token consumption from scratchpad tool returns
/// and tokens diverted from the context window by the scratchpad wrapper.
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// Total model context window in tokens.
    context_window: usize,
    /// Safety margin (0.0–1.0) reserved for model reasoning.
    safety_margin: f32,
    /// Tokens already consumed by system prompt, task message, tool schemas, etc.
    initial_used: usize,
    /// Token counter for a given provider/model.
    token_counter: Arc<dyn TokenCounter>,
    /// Estimated tokens consumed so far by scratchpad tool returns.
    estimated_used: Arc<AtomicUsize>,
    /// Total tokens of raw tool output diverted to scratchpad instead of context.
    tokens_intercepted: Arc<AtomicUsize>,
    /// Total tokens extracted from scratchpad back into context via exploration tools.
    tokens_extracted: Arc<AtomicUsize>,
    /// Maximum tokens a single extraction tool call may return.
    max_extraction_tokens: Option<usize>,
}

impl ContextBudget {
    /// Create a new context budget tracker.
    ///
    /// `context_window` is the model's total context limit in tokens.
    /// `safety_margin` is the fraction (0.0–1.0) reserved for reasoning.
    /// `initial_used` is the estimated tokens already consumed by prompts/schemas.
    /// `token_counter` is the provider-specific tokenizer.
    pub fn new(
        context_window: usize,
        safety_margin: f32,
        initial_used: usize,
        token_counter: Arc<dyn TokenCounter>,
    ) -> Self {
        Self {
            context_window,
            safety_margin: safety_margin.clamp(0.0, 0.95),
            initial_used,
            token_counter,
            estimated_used: Arc::new(AtomicUsize::new(initial_used)),
            tokens_intercepted: Arc::new(AtomicUsize::new(0)),
            tokens_extracted: Arc::new(AtomicUsize::new(0)),
            max_extraction_tokens: None,
        }
    }

    /// Set the per-call extraction token limit.
    pub fn with_max_extraction_tokens(mut self, limit: usize) -> Self {
        self.max_extraction_tokens = Some(limit);
        self
    }

    /// Get the per-call extraction token limit, if set.
    pub fn max_extraction_tokens(&self) -> Option<usize> {
        self.max_extraction_tokens
    }

    /// Usable token budget (context window minus safety margin).
    pub fn usable_budget(&self) -> usize {
        ((self.context_window as f64) * (1.0 - self.safety_margin as f64)) as usize
    }

    /// Remaining estimated tokens available.
    pub fn remaining(&self) -> usize {
        self.usable_budget()
            .saturating_sub(self.estimated_used.load(Ordering::Relaxed))
    }

    /// Count tokens for a string using the real tokenizer.
    pub fn count_tokens(&self, content: &str) -> usize {
        self.token_counter.count_tokens(content)
    }

    /// Check if content would fit within the remaining budget.
    ///
    /// Returns `Ok(estimated_tokens)` if it fits, or `Err(BudgetExceeded)` with details.
    pub fn check_fits(&self, content: &str) -> Result<usize, BudgetExceeded> {
        let tokens = self.count_tokens(content);
        let remaining = self.remaining();
        if tokens > remaining {
            Err(BudgetExceeded {
                requested_tokens: tokens,
                remaining_tokens: remaining,
                total_budget: self.usable_budget(),
            })
        } else {
            Ok(tokens)
        }
    }

    /// Atomically check if content fits and record usage in one step.
    ///
    /// Returns the token count on success, or `BudgetExceeded` if there isn't room.
    pub fn try_record_usage(&self, content: &str) -> Result<usize, BudgetExceeded> {
        let tokens = self.count_tokens(content);
        let usable = self.usable_budget();

        match self
            .estimated_used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                if current + tokens <= usable {
                    Some(current + tokens)
                } else {
                    None
                }
            }) {
            Ok(_prev) => Ok(tokens),
            Err(current) => Err(BudgetExceeded {
                requested_tokens: tokens,
                remaining_tokens: usable.saturating_sub(current),
                total_budget: usable,
            }),
        }
    }

    /// Record that tokens were consumed (after a successful tool return).
    pub fn record_usage(&self, estimated_tokens: usize) {
        self.estimated_used
            .fetch_add(estimated_tokens, Ordering::Relaxed);
    }

    /// Record tokens diverted from context to scratchpad.
    pub fn record_intercepted(&self, tokens: usize) {
        self.tokens_intercepted.fetch_add(tokens, Ordering::Relaxed);
    }

    /// Record tokens extracted from scratchpad back into context.
    pub fn record_extracted(&self, tokens: usize) {
        self.tokens_extracted.fetch_add(tokens, Ordering::Relaxed);
    }

    /// Get scratchpad usage summary: (tokens_intercepted, tokens_extracted).
    pub fn scratchpad_usage(&self) -> (usize, usize) {
        (
            self.tokens_intercepted.load(Ordering::Relaxed),
            self.tokens_extracted.load(Ordering::Relaxed),
        )
    }

    /// Update estimated usage authoritatively.
    ///
    /// For example, the LLM's `input_tokens` is the authoritative count of context size for a
    /// turn. We can store `max(current, input + output)` to reflect actual
    /// context pressure without double-counting scratchpad extraction tokens
    /// (which are already included in the LLM's next input_tokens).
    pub fn set_estimated_used(&self, input_tokens: u64, output_tokens: u64) {
        let llm_total = (input_tokens + output_tokens) as usize;
        // Atomically update to max(current, llm_total)
        let _ = self
            .estimated_used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                if llm_total > current {
                    Some(llm_total)
                } else {
                    None
                }
            });
    }

    /// Build a `window_hint` string for tool metadata.
    pub fn window_hint(&self) -> String {
        let remaining = self.remaining();
        let used = self.estimated_used.load(Ordering::Relaxed);
        let scratchpad_used = used.saturating_sub(self.initial_used);
        format!(
            "~{} tokens remaining (~{} used [{} baseline + {} scratchpad/llm-reported] of ~{} usable)",
            remaining,
            used,
            self.initial_used,
            scratchpad_used,
            self.usable_budget()
        )
    }
}

/// Error when a tool result would exceed the context budget.
#[derive(Debug, Clone)]
pub struct BudgetExceeded {
    pub requested_tokens: usize,
    pub remaining_tokens: usize,
    pub total_budget: usize,
}

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "slice_too_large: ~{} tokens requested but only ~{} remaining. \
             Use head, slice, or grep to read smaller portions.",
            self.requested_tokens, self.remaining_tokens
        )
    }
}

/// Error when a single extraction exceeds the per-call token limit.
#[derive(Debug, Clone)]
pub struct ExtractionLimitExceeded {
    pub estimated_tokens: usize,
    pub limit: usize,
}

impl std::fmt::Display for ExtractionLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "extraction_too_large: ~{} tokens exceeds the {} token per-call limit. \
             Use more targeted tools (head, slice, grep, get_in) to extract smaller portions.",
            self.estimated_tokens, self.limit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple test counter that returns chars / 4 (for predictable test behavior).
    struct TestCounter;

    impl TokenCounter for TestCounter {
        fn count_tokens(&self, text: &str) -> usize {
            text.len() / 4
        }
    }

    fn test_counter() -> Arc<dyn TokenCounter> {
        Arc::new(TestCounter)
    }

    #[test]
    fn test_basic_budget() {
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        let usable = budget.usable_budget();
        // f32 precision: 0.20 as f32 -> f64 introduces tiny error
        assert!((79_999..=80_000).contains(&usable));
        assert_eq!(budget.remaining(), usable);
    }

    #[test]
    fn test_record_usage() {
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        let usable = budget.usable_budget();
        budget.record_usage(10_000);
        assert_eq!(budget.remaining(), usable - 10_000);
    }

    #[test]
    fn test_check_fits_ok() {
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        // 400 chars = ~100 tokens with TestCounter
        let content = "x".repeat(400);
        assert!(budget.check_fits(&content).is_ok());
    }

    #[test]
    fn test_check_fits_exceeded() {
        let budget = ContextBudget::new(1000, 0.20, 0, test_counter());
        let usable = budget.usable_budget();
        budget.record_usage(usable - 10);
        // 800 chars = ~200 tokens, but only ~10 remaining
        let content = "x".repeat(800);
        let err = budget.check_fits(&content).unwrap_err();
        assert_eq!(err.requested_tokens, 200);
        assert_eq!(err.remaining_tokens, 10);
    }

    #[test]
    fn test_try_record_usage_success_and_accumulation() {
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        let usable = budget.usable_budget();
        // 400 chars = 100 tokens with TestCounter
        let content = "x".repeat(400);

        // First call succeeds and records
        let tokens = budget.try_record_usage(&content).unwrap();
        assert_eq!(tokens, 100);
        assert_eq!(budget.remaining(), usable - 100);

        // Second call accumulates
        let tokens = budget.try_record_usage(&content).unwrap();
        assert_eq!(tokens, 100);
        assert_eq!(budget.remaining(), usable - 200);
    }

    #[test]
    fn test_try_record_usage_exceeds_budget() {
        let budget = ContextBudget::new(1000, 0.20, 0, test_counter());
        let usable = budget.usable_budget();
        // Fill most of the budget
        budget.record_usage(usable - 10);

        // 800 chars = 200 tokens, but only ~10 remaining
        let big_content = "x".repeat(800);
        let err = budget.try_record_usage(&big_content).unwrap_err();
        assert_eq!(err.requested_tokens, 200);
        assert_eq!(err.remaining_tokens, 10);

        // Budget should be unchanged after failed attempt (atomic rollback)
        assert_eq!(budget.remaining(), 10);
    }

    #[test]
    fn test_count_tokens() {
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        assert_eq!(budget.count_tokens("abcd"), 1);
        assert_eq!(budget.count_tokens("abcdefgh"), 2);
        assert_eq!(budget.count_tokens(""), 0);
    }

    #[test]
    fn test_window_hint_with_baseline() {
        let budget = ContextBudget::new(100_000, 0.20, 2000, test_counter());
        budget.record_usage(3000);
        let hint = budget.window_hint();
        assert!(hint.contains("tokens remaining"));
        assert!(hint.contains("5000 used"));
        assert!(hint.contains("2000 baseline"));
        assert!(hint.contains("3000 scratchpad"));
    }

    #[test]
    fn test_initial_used_reduces_remaining() {
        let budget = ContextBudget::new(100_000, 0.20, 10_000, test_counter());
        let usable = budget.usable_budget();
        // initial_used is seeded into estimated_used, so remaining is reduced
        assert_eq!(budget.remaining(), usable - 10_000);
    }

    #[test]
    fn test_tiktoken_counter_for_model() {
        let counter = TiktokenCounter::for_model("gpt-5.2");
        let tokens = counter.count_tokens("Hello, world!");
        assert!(tokens > 0);
        assert!(tokens < 10);
    }

    #[test]
    fn test_tiktoken_default_counter() {
        let counter = TiktokenCounter::default_counter();
        let tokens = counter.count_tokens("Hello, world!");
        assert!(tokens > 0);
        assert!(tokens < 10);
    }

    #[test]
    fn test_token_counter_for_provider() {
        let openai = token_counter_for_provider("openai", "gpt-5.2");
        assert!(openai.count_tokens("test") > 0);

        let anthropic = token_counter_for_provider("anthropic", "claude-3-opus");
        assert!(anthropic.count_tokens("test") > 0);

        let ollama = token_counter_for_provider("ollama", "llama3");
        assert!(ollama.count_tokens("test") > 0);
    }

    #[test]
    fn test_per_agent_budgets_are_independent() {
        // Each agent gets its own ContextBudget::new() — verify no shared state
        let worker1 = ContextBudget::new(100_000, 0.20, 1000, test_counter());
        let worker2 = ContextBudget::new(100_000, 0.20, 1000, test_counter());

        worker1.record_usage(5000);
        worker1.record_intercepted(500);
        worker1.record_extracted(200);

        // Worker 2 is unaffected
        assert_eq!(worker2.remaining(), worker2.usable_budget() - 1000);
        assert_eq!(worker2.scratchpad_usage(), (0, 0));

        // Worker 1 reflects its own usage
        assert_eq!(worker1.remaining(), worker1.usable_budget() - 6000);
        assert_eq!(worker1.scratchpad_usage(), (500, 200));
    }

    #[test]
    fn test_per_agent_different_context_windows() {
        // Workers can have different context windows
        let worker_small = ContextBudget::new(50_000, 0.20, 1000, test_counter());
        let worker_large = ContextBudget::new(200_000, 0.20, 1000, test_counter());

        assert!(worker_large.usable_budget() > worker_small.usable_budget());
        assert!(worker_large.remaining() > worker_small.remaining());
    }

    #[test]
    fn test_set_estimated_used_bumps_up_when_llm_reports_more() {
        // initial_used=1000 → estimated_used starts at 1000.
        let budget = ContextBudget::new(100_000, 0.20, 1000, test_counter());
        let initial_remaining = budget.remaining();

        // LLM reports 5000 input + 200 output = 5200 total. That's higher
        // than the 1000 baseline, so estimated_used should jump to 5200.
        budget.set_estimated_used(5000, 200);
        assert!(
            budget.remaining() < initial_remaining,
            "remaining must drop after LLM reports higher usage"
        );
        assert_eq!(
            budget.usable_budget() - budget.remaining(),
            5200,
            "estimated_used should be max(prev, llm input+output)"
        );
    }

    #[test]
    fn test_set_estimated_used_does_not_lower_estimate() {
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        // Push usage up via record_usage first.
        budget.record_usage(8000);
        let after_record = budget.remaining();

        // LLM reports a smaller total — must NOT lower the estimate
        // (otherwise we'd "leak" budget back).
        budget.set_estimated_used(1000, 500);
        assert_eq!(
            budget.remaining(),
            after_record,
            "set_estimated_used must never decrease estimated_used"
        );
    }

    #[test]
    fn test_set_estimated_used_is_independent_of_scratchpad_counters() {
        // LLM ground-truth feedback feeds estimated_used; tokens_intercepted
        // and tokens_extracted are tracked separately and must stay untouched.
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        budget.record_intercepted(500);
        budget.record_extracted(200);
        budget.set_estimated_used(10_000, 1_000);

        assert_eq!(budget.scratchpad_usage(), (500, 200));
    }

    #[test]
    fn test_set_estimated_used_with_zero_is_noop() {
        // First push estimated_used up via record_usage so we can verify
        // that set_estimated_used(0,0) doesn't reset it (max-only update).
        let budget = ContextBudget::new(100_000, 0.20, 0, test_counter());
        budget.record_usage(2_000);
        let before = budget.remaining();
        budget.set_estimated_used(0, 0);
        assert_eq!(budget.remaining(), before);
    }
}
