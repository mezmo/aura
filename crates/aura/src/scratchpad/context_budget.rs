//! Context budget tracking for scratchpad tools.
//!
//! Tracks estimated token usage from scratchpad tool returns and provides
//! remaining-budget hints to prevent context window overflow.
//!
//! Uses real tokenizers for accurate token counting via the `TokenCounter`
//! trait. Counting is provider-aware:
//!
//! - **OpenAI** — `tiktoken-rs` resolves the exact tokenizer per model.
//! - **Gemini** — `gemini-tokenizer` runs the embedded Gemma 3 SentencePiece
//!   model locally (exact, matches Google's official SDK).
//! - **Anthropic / Bedrock-Claude** — Claude ships no public tokenizer, so a
//!   calibrated `cl100k_base` approximation is used (see [`ClaudeApproxCounter`]).
//! - **Everything else** — `o200k_base` as a conservative fallback floor.
//!
//! Additional provider-specific tokenizers can be added by implementing
//! `TokenCounter` and updating `token_counter_for_provider`.

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
///
/// The wrapped `CoreBPE` is borrowed from tiktoken-rs's process-wide
/// `lazy_static` singletons (e.g. `o200k_base_singleton()`), so constructing
/// a `TiktokenCounter` is effectively free — no vocabulary parsing, no
/// HashMap allocation. The previous implementation called `o200k_base()` /
/// `get_bpe_from_model()`, which rebuild a fresh `CoreBPE` from the embedded
/// vocab file on every call (~3 MB include_str + ~200K HashMap inserts +
/// ~20–40 MB heap, ~50–200 ms). With per-request `Agent::new()` and
/// per-worker `create_worker()` paths in the hot path, that churn caused
/// noticeable RSS bloat and host-level slowdown over long-running sessions.
pub struct TiktokenCounter {
    bpe: &'static tiktoken_rs::CoreBPE,
}

impl TiktokenCounter {
    /// Create a counter for a specific model.
    /// Falls back to `o200k_base` if the model isn't recognized.
    pub fn for_model(model: &str) -> Self {
        Self {
            bpe: bpe_singleton_for_model(model),
        }
    }

    /// Create a counter using `o200k_base` (conservative default).
    pub fn default_counter() -> Self {
        Self {
            bpe: tiktoken_rs::o200k_base_singleton(),
        }
    }
}

/// Resolve a model name to the matching tiktoken singleton `CoreBPE`.
///
/// `tiktoken_rs::get_bpe_from_model` exists but builds a fresh `CoreBPE`
/// every call. The `_singleton()` variants return a `&'static CoreBPE` from
/// a `lazy_static`, so we map model → tokenizer ourselves and dispatch to
/// the right one. Unknown models fall back to `o200k_base` (matches the
/// previous default).
fn bpe_singleton_for_model(model: &str) -> &'static tiktoken_rs::CoreBPE {
    use tiktoken_rs::tokenizer::{Tokenizer, get_tokenizer};
    match get_tokenizer(model) {
        Some(Tokenizer::O200kBase) => tiktoken_rs::o200k_base_singleton(),
        Some(Tokenizer::O200kHarmony) => tiktoken_rs::o200k_harmony_singleton(),
        Some(Tokenizer::Cl100kBase) => tiktoken_rs::cl100k_base_singleton(),
        Some(Tokenizer::P50kBase) => tiktoken_rs::p50k_base_singleton(),
        Some(Tokenizer::P50kEdit) => tiktoken_rs::p50k_edit_singleton(),
        Some(Tokenizer::R50kBase) | Some(Tokenizer::Gpt2) => tiktoken_rs::r50k_base_singleton(),
        None => tiktoken_rs::o200k_base_singleton(),
    }
}

impl TokenCounter for TiktokenCounter {
    fn count_tokens(&self, text: &str) -> usize {
        self.bpe.encode_with_special_tokens(text).len()
    }
}

/// Exact local token counter for Gemini models.
///
/// Wraps `gemini-tokenizer`, which embeds the Gemma 3 SentencePiece model
/// (262,144-token vocab) and produces counts identical to Google's official
/// Python SDK. Fully local — no network or external files.
///
/// The configured model id is passed through to the tokenizer so counts follow
/// whatever vocabulary the crate maps that model to. A model id the crate
/// doesn't recognize falls back to a current Gemini model rather than failing.
pub struct GeminiCounter {
    tokenizer: gemini_tokenizer::LocalTokenizer,
}

impl GeminiCounter {
    /// Model used when the configured Gemini model isn't recognized by the
    /// tokenizer crate — a best-effort fallback so an unknown id still yields a
    /// Gemini estimate instead of erroring.
    const FALLBACK_MODEL: &'static str = "gemini-2.5-pro";

    /// Create a counter for a specific Gemini model.
    pub fn for_model(model: &str) -> Self {
        let tokenizer = gemini_tokenizer::LocalTokenizer::new(model)
            .or_else(|_| gemini_tokenizer::LocalTokenizer::new(Self::FALLBACK_MODEL))
            .expect("embedded Gemini SentencePiece model must load");
        Self { tokenizer }
    }
}

impl TokenCounter for GeminiCounter {
    fn count_tokens(&self, text: &str) -> usize {
        self.tokenizer.count_tokens(text, None).total_tokens
    }
}

/// Calibrated approximate token counter for Claude models.
///
/// Claude 3+ models do not ship a public tokenizer. This counter scales a
/// local `cl100k_base` BPE count by an empirically measured correction factor,
/// which is materially closer to Claude's true tokenization than the raw
/// `o200k_base` fallback while staying fully local and synchronous.
///
/// The count is an estimate, not an exact figure.
pub struct ClaudeApproxCounter {
    bpe: &'static tiktoken_rs::CoreBPE,
}

/// Multiplier applied to `cl100k_base` counts to approximate Claude tokenization.
///
/// Claude's tokenizer is unavailable publicly; community measurements put it at
/// roughly 10% above `cl100k_base` on typical mixed text. This is an
/// approximation, not an exact factor.
const CLAUDE_CORRECTION_FACTOR: f64 = 1.1;

impl ClaudeApproxCounter {
    /// Create a Claude approximation counter.
    pub fn new() -> Self {
        Self {
            bpe: tiktoken_rs::cl100k_base_singleton(),
        }
    }
}

impl Default for ClaudeApproxCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenCounter for ClaudeApproxCounter {
    fn count_tokens(&self, text: &str) -> usize {
        let base = self.bpe.encode_with_special_tokens(text).len();
        ((base as f64) * CLAUDE_CORRECTION_FACTOR).ceil() as usize
    }
}

/// Best-effort counter for providers without a dedicated tokenizer.
///
/// Covers Ollama, OpenRouter, non-Claude Bedrock models, and any unrecognized
/// provider. Resolves the model id against tiktoken when it is recognized,
/// otherwise `o200k_base`. These all sit close to an o200k-family tokenizer,
/// so the residual error is small — unlike Gemini and Claude, whose
/// tokenizers differ enough to warrant the dedicated counters above.
fn fallback_token_counter(model: &str) -> TiktokenCounter {
    TiktokenCounter::for_model(model)
}

/// Create a token counter for the given provider and model.
///
/// Each arm names how that provider is counted: an exact tokenizer where one
/// exists, the calibrated Claude approximation for Claude models (native or
/// Bedrock-hosted), or [`fallback_token_counter`] for everything else. To add
/// a provider-specific tokenizer, implement `TokenCounter` and add a match arm.
pub fn token_counter_for_provider(provider: &str, model: &str) -> Arc<dyn TokenCounter> {
    match provider {
        "openai" => Arc::new(TiktokenCounter::for_model(model)),
        "gemini" => Arc::new(GeminiCounter::for_model(model)),
        // Only Claude ids match — Bedrock's non-Claude vendors fall through.
        "anthropic" | "bedrock" if model.contains("claude") => Arc::new(ClaudeApproxCounter::new()),
        _ => Arc::new(fallback_token_counter(model)),
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

    /// Invariants any real tokenizer must honor, independent of provider or
    /// the specific vocabulary behind it.
    fn assert_counter_invariants(counter: &dyn TokenCounter) {
        assert_eq!(counter.count_tokens(""), 0, "empty text is zero tokens");
        assert!(
            counter.count_tokens("hello world") > 0,
            "non-empty text is a positive number of tokens"
        );
        let short = counter.count_tokens("the quick brown fox");
        let long = counter.count_tokens("the quick brown fox jumps over the lazy dog repeatedly");
        assert!(long > short, "more text yields more tokens");
    }

    #[test]
    fn test_token_counter_for_provider() {
        // Every provider/model — including unknown ones — must yield a usable counter.
        let cases = [
            ("openai", "gpt-4o"),
            ("gemini", "gemini-2.5-pro"),
            // Unrecognized Gemini id must hit the fallback, not error.
            ("gemini", "gemini-9.9-not-a-real-model"),
            ("anthropic", "claude-3-5-sonnet"),
            ("bedrock", "us.anthropic.claude-3-5-sonnet-20241022-v2:0"),
            ("bedrock", "meta.llama3-70b-instruct-v1:0"),
            ("ollama", "llama3"),
            ("openrouter", "anthropic/claude-3-opus"),
            ("some-future-provider", "whatever-model"),
        ];
        for (provider, model) in cases {
            assert_counter_invariants(&*token_counter_for_provider(provider, model));
        }
    }

    #[test]
    fn test_bedrock_routing_by_model_id() {
        let text = "tokenize this representative prompt for comparison";
        let claude =
            token_counter_for_provider("bedrock", "us.anthropic.claude-3-5-sonnet-20241022-v2:0");
        let llama = token_counter_for_provider("bedrock", "meta.llama3-70b-instruct-v1:0");
        assert_eq!(
            claude.count_tokens(text),
            ClaudeApproxCounter::new().count_tokens(text)
        );
        assert_eq!(
            llama.count_tokens(text),
            fallback_token_counter("meta.llama3-70b-instruct-v1:0").count_tokens(text)
        );
    }

    #[test]
    fn test_bedrock_claude_uses_anthropic_counter() {
        let text = "Count these tokens the Claude way, whichever provider hosts the model.";
        let bedrock =
            token_counter_for_provider("bedrock", "us.anthropic.claude-3-5-sonnet-20241022-v2:0");
        let anthropic = token_counter_for_provider("anthropic", "claude-3-5-sonnet");
        assert_eq!(bedrock.count_tokens(text), anthropic.count_tokens(text));
    }

    #[test]
    fn test_claude_counter_not_below_cl100k_baseline() {
        // Holds for any correction factor >= 1, so it doesn't pin the factor value.
        let claude = ClaudeApproxCounter::new();
        let baseline = tiktoken_rs::cl100k_base_singleton();
        for text in [
            "fn main() { println!(\"hi\"); }",
            "The quick brown fox jumps over the lazy dog.",
            "λx. x + 1  — unicode and whitespace   spread out",
        ] {
            let bpe = baseline.encode_with_special_tokens(text).len();
            assert!(
                claude.count_tokens(text) >= bpe,
                "Claude approximation must not fall below its BPE baseline for {text:?}"
            );
        }
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
