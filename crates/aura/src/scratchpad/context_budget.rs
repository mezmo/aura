//! Context budget tracking for scratchpad tools.
//!
//! Tracks estimated token usage from scratchpad tool returns and provides
//! remaining-budget hints to prevent context window overflow.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Tracks estimated token consumption from scratchpad tool returns
/// and bytes diverted from the context window by the scratchpad wrapper.
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// Total model context window in tokens.
    context_window: usize,
    /// Safety margin (0.0–1.0) reserved for model reasoning.
    safety_margin: f32,
    /// Estimated tokens consumed so far by scratchpad tool returns.
    estimated_used: Arc<AtomicUsize>,
    /// Total bytes of raw tool output diverted to scratchpad instead of context.
    bytes_intercepted: Arc<AtomicUsize>,
    /// Total bytes extracted from scratchpad back into context via exploration tools.
    bytes_extracted: Arc<AtomicUsize>,
}

impl ContextBudget {
    /// Create a new context budget tracker.
    ///
    /// `context_window` is the model's total context limit in tokens.
    /// `safety_margin` is the fraction (0.0–1.0) reserved for reasoning.
    pub fn new(context_window: usize, safety_margin: f32) -> Self {
        Self {
            context_window,
            safety_margin: safety_margin.clamp(0.0, 0.95),
            estimated_used: Arc::new(AtomicUsize::new(0)),
            bytes_intercepted: Arc::new(AtomicUsize::new(0)),
            bytes_extracted: Arc::new(AtomicUsize::new(0)),
        }
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

    /// Estimate tokens for a string (chars / 4).
    pub fn estimate_tokens(content: &str) -> usize {
        content.len() / 4
    }

    /// Check if content would fit within the remaining budget.
    ///
    /// Returns `Ok(estimated_tokens)` if it fits, or `Err(BudgetExceeded)` with details.
    pub fn check_fits(&self, content: &str) -> Result<usize, BudgetExceeded> {
        let tokens = Self::estimate_tokens(content);
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

    /// Record that tokens were consumed (after a successful tool return).
    pub fn record_usage(&self, estimated_tokens: usize) {
        self.estimated_used
            .fetch_add(estimated_tokens, Ordering::Relaxed);
    }

    /// Record raw bytes diverted from context to scratchpad.
    pub fn record_intercepted(&self, bytes: usize) {
        self.bytes_intercepted.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record bytes extracted from scratchpad back into context.
    pub fn record_extracted(&self, bytes: usize) {
        self.bytes_extracted.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Get scratchpad usage summary: (bytes_intercepted, bytes_extracted).
    pub fn scratchpad_usage(&self) -> (usize, usize) {
        (
            self.bytes_intercepted.load(Ordering::Relaxed),
            self.bytes_extracted.load(Ordering::Relaxed),
        )
    }

    /// Build a `window_hint` string for tool metadata.
    pub fn window_hint(&self) -> String {
        let remaining = self.remaining();
        let used = self.estimated_used.load(Ordering::Relaxed);
        format!(
            "~{} tokens remaining (~{} used of ~{} usable)",
            remaining,
            used,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_budget() {
        let budget = ContextBudget::new(100_000, 0.20);
        let usable = budget.usable_budget();
        // f32 precision: 0.20 as f32 -> f64 introduces tiny error
        assert!((79_999..=80_000).contains(&usable));
        assert_eq!(budget.remaining(), usable);
    }

    #[test]
    fn test_record_usage() {
        let budget = ContextBudget::new(100_000, 0.20);
        let usable = budget.usable_budget();
        budget.record_usage(10_000);
        assert_eq!(budget.remaining(), usable - 10_000);
    }

    #[test]
    fn test_check_fits_ok() {
        let budget = ContextBudget::new(100_000, 0.20);
        // 400 chars = ~100 tokens
        let content = "x".repeat(400);
        assert!(budget.check_fits(&content).is_ok());
    }

    #[test]
    fn test_check_fits_exceeded() {
        let budget = ContextBudget::new(1000, 0.20);
        let usable = budget.usable_budget();
        budget.record_usage(usable - 10);
        // 800 chars = ~200 tokens, but only ~10 remaining
        let content = "x".repeat(800);
        let err = budget.check_fits(&content).unwrap_err();
        assert_eq!(err.requested_tokens, 200);
        assert_eq!(err.remaining_tokens, 10);
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(ContextBudget::estimate_tokens("abcd"), 1);
        assert_eq!(ContextBudget::estimate_tokens("abcdefgh"), 2);
        assert_eq!(ContextBudget::estimate_tokens(""), 0);
    }

    #[test]
    fn test_window_hint() {
        let budget = ContextBudget::new(100_000, 0.20);
        budget.record_usage(5000);
        let hint = budget.window_hint();
        assert!(hint.contains("tokens remaining"));
        assert!(hint.contains("5000 used"));
    }
}
