//! Streaming request hook for request lifecycle management.
//!
//! This hook manages the full lifecycle of streaming requests:
//! 1. Timeout/cancellation - External cancellation signal (e.g., client disconnect)
//! 2. Tool event emission - `aura.tool_requested`, `aura.tool_usage` events
//! 3. Usage state tracking - Token counts for billing/metrics
//! 4. Tool ID FIFO correlation - Associates tool calls with results
//!
//! # Tool Event Flow
//!
//! - `on_tool_call`: Pushes tool_call_id to FIFO queue, emits `aura.tool_requested`
//! - MCP execution: Peeks tool_call_id for `aura.tool_start` event
//! - `on_tool_result`: Pops tool_call_id from queue (cleanup), adds to pending_tool_ids
//! - `on_stream_completion_response_finish`: Captures usage, emits `aura.tool_usage` for pending tools
//!
//! This relies on Rig's streaming mode executing tools sequentially.
//! See `docs/rig-tool-execution-order.md` for analysis.
//!
//! # Usage
//!
//! ```ignore
//! let (hook, cancel_sender, usage_state) = StreamingRequestHook::new(Duration::from_secs(60), "req_123");
//!
//! // Pass hook to streaming request
//! agent.stream_prompt(query).with_hook(hook).multi_turn(depth).await;
//!
//! // To cancel externally (e.g., on client disconnect):
//! let _ = cancel_sender.send(true);
//!
//! // At stream end, read final usage from usage_state
//! let (prompt, completion, total) = usage_state.get_final_usage();
//! ```

use crate::scratchpad::ContextBudget;
use crate::tool_event_broker::{
    pop_tool_call_id, publish_tool_requested, publish_tool_usage, push_tool_call_id,
};
use rig::agent::{CancelSignal, StreamingPromptHook};
use rig::completion::{CompletionModel, GetTokenUsage, Message};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Maximum pending tool IDs before warning. Prevents unbounded growth if
/// usage events never fire (e.g., provider doesn't return token counts).
const MAX_PENDING_TOOL_IDS: usize = 256;

/// Shared usage state that survives hook cloning.
///
/// Tracks context window position across multi-turn tool call loops.
/// The UI needs to know context size for warning thresholds.
///
/// Context window = what will be sent in the NEXT request:
/// - `initial_prompt_tokens`: First LLM turn's input (system + history + user message)
/// - `accumulated_completion_tokens`: SUM of all LLM turn outputs (all streamed to frontend)
/// - `tool_completion_tokens`: Subset of accumulated_completion spent generating tool call JSON
///
/// All completion tokens are accumulated because everything streams to the frontend
/// and gets stored in the thread history for the next request.
#[derive(Clone, Default)]
pub struct UsageState {
    /// Whether initial_prompt_tokens has been set (first turn captured)
    initialized: Arc<AtomicBool>,
    /// Prompt tokens from the FIRST LLM turn (system + history + user message)
    initial_prompt_tokens: Arc<AtomicU64>,
    /// Accumulated completion tokens across ALL LLM turns (all stream to frontend)
    accumulated_completion_tokens: Arc<AtomicU64>,
    /// Completion tokens spent on tool-call turns (subset of accumulated_completion_tokens)
    tool_completion_tokens: Arc<AtomicU64>,
    /// Tool IDs completed since the last usage event (for aura.tool_usage correlation)
    pending_tool_ids: Arc<Mutex<Vec<String>>>,
}

impl UsageState {
    /// Create a new empty usage state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the final usage values for context window calculation.
    ///
    /// Returns (prompt_tokens, completion_tokens, total_tokens) where:
    /// - prompt_tokens: Initial input from first LLM turn (system + history + user msg)
    /// - completion_tokens: Accumulated output across ALL LLM turns (all stream to frontend)
    /// - total_tokens: prompt + completion = context window position for next request
    ///
    /// This gives accurate context tracking since all completions stream to frontend
    /// and get stored in thread history.
    pub fn get_final_usage(&self) -> (u64, u64, u64) {
        let prompt = self.initial_prompt_tokens.load(Ordering::Acquire);
        let completion = self.accumulated_completion_tokens.load(Ordering::Acquire);
        (prompt, completion, prompt + completion)
    }

    /// Get the completion tokens spent on tool-call turns.
    ///
    /// This is a subset of the total completion tokens returned by `get_final_usage()`.
    /// The response completion tokens can be derived as `completion - tool_completion`.
    pub fn get_tool_completion_tokens(&self) -> u64 {
        self.tool_completion_tokens.load(Ordering::Acquire)
    }

    /// Store usage values from a completion response.
    ///
    /// On first call: captures initial_prompt_tokens (the actual frontend input).
    /// On each call: accumulates completion tokens (all LLM output streams to frontend).
    /// When `is_tool_turn` is true, also accumulates into the tool completion counter.
    ///
    /// The `_total` parameter is ignored - we calculate total as prompt + accumulated_completion.
    pub fn store_usage(&self, prompt: u64, completion: u64, _total: u64, is_tool_turn: bool) {
        // Only set initial_prompt_tokens on first call
        if !self.initialized.swap(true, Ordering::AcqRel) {
            self.initial_prompt_tokens.store(prompt, Ordering::Release);
            tracing::debug!(
                "Captured initial prompt tokens: {} (first LLM turn)",
                prompt
            );
        }

        // Accumulate all completion tokens (everything streams to frontend)
        let prev = self
            .accumulated_completion_tokens
            .fetch_add(completion, Ordering::AcqRel);
        tracing::debug!(
            "Accumulated completion tokens: {} + {} = {} (tool_turn={})",
            prev,
            completion,
            prev + completion,
            is_tool_turn,
        );

        // Track tool-call completion tokens separately
        if is_tool_turn {
            self.tool_completion_tokens
                .fetch_add(completion, Ordering::AcqRel);
        }
    }

    /// Accumulate usage additively across multiple LLM calls.
    ///
    /// Unlike [`store_usage`](Self::store_usage), which captures only the *first*
    /// turn's prompt, this adds to both prompt and completion counters. Use when
    /// the caller already aggregates usage across independent LLM calls (e.g. the
    /// orchestrator summing planning, workers, synthesis, and evaluation turns)
    /// and needs the final `aura.usage` event to reflect *total billed* tokens
    /// rather than a single-turn snapshot.
    ///
    /// Marks the state as initialized so the stream handler emits `aura.usage`
    /// even when prompt was only ever accumulated through this method.
    pub fn accumulate_usage(&self, prompt: u64, completion: u64) {
        self.initialized.store(true, Ordering::Release);
        self.initial_prompt_tokens
            .fetch_add(prompt, Ordering::AcqRel);
        self.accumulated_completion_tokens
            .fetch_add(completion, Ordering::AcqRel);
    }

    /// Add a tool ID to the pending list.
    ///
    /// Called from on_tool_result when a tool completes.
    pub fn add_pending_tool_id(&self, tool_id: String) {
        match self.pending_tool_ids.lock() {
            Ok(mut pending) => {
                if pending.len() >= MAX_PENDING_TOOL_IDS {
                    tracing::warn!(
                        "pending_tool_ids at capacity ({}), dropping oldest",
                        MAX_PENDING_TOOL_IDS
                    );
                    pending.remove(0); // Drop oldest to make room
                }
                pending.push(tool_id);
            }
            Err(poisoned) => {
                // Recover from poisoned mutex - this only happens if a thread panicked
                // while holding the lock. We log the error and recover the data.
                tracing::error!(
                    "UsageState mutex poisoned in add_pending_tool_id - recovering. \
                     This indicates a prior panic during tool tracking."
                );
                let mut pending = poisoned.into_inner();
                pending.push(tool_id);
            }
        }
    }

    /// Take all pending tool IDs, leaving the list empty.
    ///
    /// Called when usage becomes available to associate tools with usage snapshot.
    pub fn take_pending_tool_ids(&self) -> Vec<String> {
        match self.pending_tool_ids.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(poisoned) => {
                // Recover from poisoned mutex - take the data and clear the list
                tracing::error!(
                    "UsageState mutex poisoned in take_pending_tool_ids - recovering. \
                     This indicates a prior panic during tool tracking."
                );
                let mut pending = poisoned.into_inner();
                std::mem::take(&mut *pending)
            }
        }
    }
}

/// Shared container for the final response content captured during streaming.
///
/// **Why this exists:** Bridges the stream processing loop and OTel span
/// recording across an async boundary. The stream loop (in `handlers.rs`)
/// captures the final accumulated response text when it sees
/// `StreamItem::Final`, and the OTel recorder (in `main.rs`
/// `StreamOtelContext::record_output()`) reads it after the stream ends to
/// set `output.value` on the `agent.stream` span.
///
/// **Without OTel this type is unnecessary** — it exists solely to carry
/// response content from the stream consumer to the span recorder.
///
/// - **Write site:** `handlers.rs`, `process_sse_stream_full` on `StreamItem::Final`
/// - **Read site:** `main.rs`, `StreamOtelContext::record_output()` after stream ends
#[derive(Clone, Default)]
pub struct ResponseContent {
    inner: Arc<Mutex<Option<String>>>,
}

impl ResponseContent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the final accumulated response content.
    pub fn set(&self, content: String) {
        match self.inner.lock() {
            Ok(mut slot) => *slot = Some(content),
            Err(poisoned) => {
                tracing::error!("ResponseContent mutex poisoned in set - recovering.");
                let mut slot = poisoned.into_inner();
                *slot = Some(content);
            }
        }
    }

    /// Get the final accumulated response content, if available.
    ///
    /// Returns `None` if the stream ended before producing a `Final` item
    /// (e.g., client disconnect or timeout).
    pub fn get(&self) -> Option<String> {
        match self.inner.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => {
                tracing::error!("ResponseContent mutex poisoned in get - recovering.");
                poisoned.into_inner().clone()
            }
        }
    }
}

/// Hook for managing streaming request lifecycle.
///
/// Handles timeout/cancellation, tool event emission, and usage tracking.
/// Checked at key points during streaming:
/// - Before each LLM completion call
/// - On each text delta
/// - Before each tool call (emits aura.tool_requested, registers tool_call_id)
/// - After each tool result (adds tool_id to pending_tool_ids)
/// - After each streaming completion (captures usage, emits aura.tool_usage)
///
/// Cancellation happens between operations, not mid-tool execution.
/// MCP cancellation is handled separately via client-level tracking (Arc-based).
#[derive(Clone)]
pub struct StreamingRequestHook {
    start_time: Instant,
    timeout: Duration,
    /// External cancellation signal (e.g., from client disconnect)
    cancelled: watch::Receiver<bool>,
    /// Request ID for event correlation
    request_id: String,
    /// Shared usage state (returned separately for handler access)
    usage_state: UsageState,
    /// Optional per-agent scratchpad budget. When set, the hook feeds the
    /// LLM-reported per-turn input/output tokens into the budget as ground
    /// truth so `remaining()` reflects actual context pressure.
    scratchpad_budget: Option<ContextBudget>,
}

impl StreamingRequestHook {
    /// Create a new streaming request hook with the given timeout duration and request ID.
    ///
    /// Returns a tuple of (hook, cancel_sender, usage_state).
    /// - `hook`: The hook to pass to stream_prompt().with_hook()
    /// - `cancel_sender`: Send `true` to trigger cancellation
    /// - `usage_state`: Shared state - handler keeps clone to read final usage at stream end
    pub fn new(
        timeout: Duration,
        request_id: impl Into<String>,
    ) -> (Self, watch::Sender<bool>, UsageState) {
        Self::with_scratchpad_budget(timeout, request_id, None)
    }

    /// Like `new`, but additionally wires a scratchpad `ContextBudget` so the
    /// hook can feed LLM-reported per-turn token counts back into the budget
    /// after each completion turn (mirrors what orchestration workers do via
    /// `StreamItem::TurnUsage`).
    pub fn with_scratchpad_budget(
        timeout: Duration,
        request_id: impl Into<String>,
        scratchpad_budget: Option<ContextBudget>,
    ) -> (Self, watch::Sender<bool>, UsageState) {
        let (tx, rx) = watch::channel(false);
        let usage_state = UsageState::new();
        let hook = Self {
            start_time: Instant::now(),
            timeout,
            cancelled: rx,
            request_id: request_id.into(),
            usage_state: usage_state.clone(),
            scratchpad_budget,
        };
        (hook, tx, usage_state)
    }

    /// Check if the request should be cancelled (timeout or external signal).
    fn should_cancel(&self) -> bool {
        if *self.cancelled.borrow() {
            return true;
        }
        self.start_time.elapsed() > self.timeout
    }

    /// Check and cancel if needed, logging the reason.
    fn check_and_cancel(&self, cancel_sig: CancelSignal, context: &str) {
        if *self.cancelled.borrow() {
            tracing::info!("Request cancelled externally during {}", context);
            cancel_sig.cancel();
        } else if self.start_time.elapsed() > self.timeout {
            tracing::warn!(
                "Request timeout ({:?}) exceeded during {} - cancelling",
                self.timeout,
                context
            );
            cancel_sig.cancel();
        }
    }
}

// Allow manual_async_fn because we're matching the Rig trait's return type pattern
#[allow(clippy::manual_async_fn)]
impl<M> StreamingPromptHook<M> for StreamingRequestHook
where
    M: CompletionModel,
{
    fn on_completion_call(
        &self,
        _prompt: &Message,
        _history: &[Message],
        cancel_sig: CancelSignal,
    ) -> impl Future<Output = ()> + Send {
        async move {
            self.check_and_cancel(cancel_sig, "completion call");
        }
    }

    fn on_text_delta(
        &self,
        _text_delta: &str,
        _aggregated_text: &str,
        cancel_sig: CancelSignal,
    ) -> impl Future<Output = ()> + Send {
        async move {
            // Only check periodically for text deltas (they're frequent)
            if self.should_cancel() {
                self.check_and_cancel(cancel_sig, "text streaming");
            }
        }
    }

    fn on_tool_call_delta(
        &self,
        _tool_call_id: &str,
        _tool_call_name: Option<&str>,
        _tool_call_delta: &str,
        cancel_sig: CancelSignal,
    ) -> impl Future<Output = ()> + Send {
        async move {
            if self.should_cancel() {
                self.check_and_cancel(cancel_sig, "tool call delta");
            }
        }
    }

    fn on_tool_call(
        &self,
        tool_name: &str,
        id: Option<String>,
        args: &str,
        cancel_sig: CancelSignal,
    ) -> impl Future<Output = ()> + Send {
        let tool_name = tool_name.to_string();
        let request_id = self.request_id.clone();
        let tool_call_id = id;
        let args_str = args.to_string();
        async move {
            // Parse args as JSON (fallback to empty object if invalid)
            let arguments: serde_json::Value =
                serde_json::from_str(&args_str).unwrap_or(serde_json::json!({}));

            // Rig 0.28+ passes correct tool_call_id; register for event correlation
            if let Some(id) = &tool_call_id {
                push_tool_call_id(&request_id, id.clone()).await;
                publish_tool_requested(&request_id, id.clone(), tool_name.clone(), arguments).await;
            } else {
                tracing::warn!(
                    "Tool '{}' called without tool_call_id for request '{}' - event correlation unavailable",
                    tool_name,
                    request_id
                );
            }

            tracing::debug!(
                "Tool '{}' requested for request '{}' (tool_call_id: {:?})",
                tool_name,
                request_id,
                tool_call_id
            );

            if self.should_cancel() {
                tracing::info!("Cancelling before tool '{}' execution", tool_name);
                self.check_and_cancel(cancel_sig, &format!("tool call ({})", tool_name));
            }
        }
    }

    fn on_tool_result(
        &self,
        tool_name: &str,
        id: Option<String>,
        _args: &str,
        _result: &str,
        cancel_sig: CancelSignal,
    ) -> impl Future<Output = ()> + Send {
        let tool_name = tool_name.to_string();
        let request_id = self.request_id.clone();
        let tool_call_id = id.clone();
        let had_tool_call_id = id.is_some();
        let usage_state = self.usage_state.clone();

        async move {
            // Note: error status is NOT set on Rig's execute_tool span here.
            // Tool errors are captured on the child mcp.tool_call span by
            // mcp_tool_execution.rs::record_tool_call_result(), which is the
            // canonical TOOL span for Phoenix.

            // Only pop if on_tool_call pushed (i.e., tool_call_id was Some).
            // This maintains push/pop symmetry and prevents popping IDs belonging
            // to other tool calls when a tool arrives without an ID.
            if had_tool_call_id && pop_tool_call_id(&request_id).await.is_none() {
                tracing::warn!(
                    "Queue desync: pop returned None for tool '{}' on request '{}' \
                     (possible duplicate on_tool_result or Rig version issue)",
                    tool_name,
                    request_id
                );
            }

            // Add tool_id to pending list for usage association
            // This allows us to correlate tools with the usage snapshot when
            // on_stream_completion_response_finish fires
            if let Some(id) = tool_call_id {
                usage_state.add_pending_tool_id(id);
            }

            tracing::debug!(
                "Tool '{}' completed (had_tool_call_id: {})",
                tool_name,
                had_tool_call_id
            );

            if self.should_cancel() {
                tracing::info!("Cancelling after tool '{}' result", tool_name);
                self.check_and_cancel(cancel_sig, &format!("tool result ({})", tool_name));
            }
        }
    }

    fn on_stream_completion_response_finish(
        &self,
        _prompt: &Message,
        response: &M::StreamingResponse,
        _cancel_sig: CancelSignal,
    ) -> impl Future<Output = ()> + Send
    where
        M::StreamingResponse: GetTokenUsage,
    {
        let usage_state = self.usage_state.clone();
        let request_id = self.request_id.clone();
        let scratchpad_budget = self.scratchpad_budget.clone();

        // Extract usage if the response type supports it
        // StreamingResponse implements GetTokenUsage which has token_usage()
        let usage = response.token_usage();

        async move {
            if let Some(usage) = usage {
                // Emit aura.tool_usage for pending tools
                // Take pending tool IDs BEFORE store_usage so we know if this is a tool turn
                let tool_ids = usage_state.take_pending_tool_ids();
                let is_tool_turn = !tool_ids.is_empty();

                // Store usage for handler to read at stream end
                // Note: Rig uses input_tokens/output_tokens, we normalize to prompt/completion
                usage_state.store_usage(
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.total_tokens,
                    is_tool_turn,
                );

                // Feed LLM ground-truth into the scratchpad budget so its
                // remaining-budget hints reflect real context pressure (the
                // intercept/extract counters keep their own totals).
                if let Some(ref budget) = scratchpad_budget {
                    budget.set_estimated_used(usage.input_tokens, usage.output_tokens);
                }

                if is_tool_turn {
                    tracing::debug!(
                        "Publishing tool_usage for {} tools: {:?}",
                        tool_ids.len(),
                        tool_ids
                    );
                    publish_tool_usage(
                        &request_id,
                        tool_ids,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.total_tokens,
                    )
                    .await;
                }

                tracing::info!(
                    request_id = %request_id,
                    prompt_tokens = usage.input_tokens,
                    completion_tokens = usage.output_tokens,
                    total_tokens = usage.total_tokens,
                    "Token usage captured"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_request_hook_creation() {
        let (hook, _tx, _usage_state) =
            StreamingRequestHook::new(Duration::from_secs(60), "test_req_1");
        assert!(!hook.should_cancel());
        assert_eq!(hook.request_id, "test_req_1");
    }

    #[test]
    fn test_external_cancellation() {
        let (hook, tx, _usage_state) =
            StreamingRequestHook::new(Duration::from_secs(60), "test_req_2");
        assert!(!hook.should_cancel());

        // Signal cancellation
        tx.send(true).unwrap();
        assert!(hook.should_cancel());
    }

    #[test]
    fn test_timeout_detection() {
        // Create hook with very short timeout
        let (hook, _tx, _usage_state) =
            StreamingRequestHook::new(Duration::from_millis(1), "test_req_3");

        // Wait for timeout
        std::thread::sleep(Duration::from_millis(5));
        assert!(hook.should_cancel());
    }

    #[test]
    fn test_usage_state_creation() {
        let (_hook, _tx, usage_state) =
            StreamingRequestHook::new(Duration::from_secs(60), "test_req_4");

        // Initially all zeros
        let (prompt, completion, total) = usage_state.get_final_usage();
        assert_eq!(prompt, 0);
        assert_eq!(completion, 0);
        assert_eq!(total, 0);
        assert_eq!(usage_state.get_tool_completion_tokens(), 0);
    }

    #[test]
    fn test_usage_state_store_and_retrieve() {
        let usage_state = UsageState::new();

        usage_state.store_usage(1000, 200, 1200, false);

        let (prompt, completion, total) = usage_state.get_final_usage();
        assert_eq!(prompt, 1000);
        assert_eq!(completion, 200);
        assert_eq!(total, 1200);
        assert_eq!(usage_state.get_tool_completion_tokens(), 0);
    }

    #[test]
    fn test_usage_state_accumulate_aggregates_prompt_and_completion() {
        let usage_state = UsageState::new();

        // Multiple calls (as orchestration emits across planning/workers/synthesis)
        usage_state.accumulate_usage(100, 50);
        usage_state.accumulate_usage(400, 200);
        usage_state.accumulate_usage(250, 75);

        let (prompt, completion, total) = usage_state.get_final_usage();
        assert_eq!(prompt, 750, "prompt should be the sum of all inputs");
        assert_eq!(
            completion, 325,
            "completion should be the sum of all outputs"
        );
        assert_eq!(total, 1075);
    }

    #[test]
    fn test_usage_state_accumulate_marks_initialized() {
        let usage_state = UsageState::new();
        // Before any usage, get_final_usage returns 0 — handler would skip aura.usage.
        assert_eq!(usage_state.get_final_usage(), (0, 0, 0));

        usage_state.accumulate_usage(500, 100);
        let (prompt, _, _) = usage_state.get_final_usage();
        assert!(
            prompt > 0,
            "handler uses prompt > 0 to gate aura.usage emission"
        );
    }

    #[test]
    fn test_usage_state_pending_tool_ids() {
        let usage_state = UsageState::new();

        usage_state.add_pending_tool_id("call_abc".to_string());
        usage_state.add_pending_tool_id("call_def".to_string());

        let tool_ids = usage_state.take_pending_tool_ids();
        assert_eq!(tool_ids, vec!["call_abc", "call_def"]);

        // After take, list should be empty
        let tool_ids = usage_state.take_pending_tool_ids();
        assert!(tool_ids.is_empty());
    }

    #[test]
    fn test_usage_state_shared_between_clones() {
        let (_hook, _tx, usage_state) =
            StreamingRequestHook::new(Duration::from_secs(60), "test_req_5");
        let usage_state_clone = usage_state.clone();

        // Modify through original (tool turn)
        usage_state.store_usage(500, 100, 600, true);

        // Read through clone - should see the same values including tool completion
        let (prompt, completion, total) = usage_state_clone.get_final_usage();
        assert_eq!(prompt, 500);
        assert_eq!(completion, 100);
        assert_eq!(total, 600);
        assert_eq!(usage_state_clone.get_tool_completion_tokens(), 100);
    }

    #[test]
    fn test_pending_tool_ids_capacity_limit() {
        let usage_state = UsageState::new();

        // Fill to capacity
        for i in 0..MAX_PENDING_TOOL_IDS {
            usage_state.add_pending_tool_id(format!("call_{}", i));
        }

        // Add one more - should drop oldest
        usage_state.add_pending_tool_id("call_overflow".to_string());

        let tool_ids = usage_state.take_pending_tool_ids();
        assert_eq!(tool_ids.len(), MAX_PENDING_TOOL_IDS);
        assert_eq!(tool_ids[0], "call_1"); // First one was dropped
        assert_eq!(tool_ids[MAX_PENDING_TOOL_IDS - 1], "call_overflow");
    }

    #[test]
    fn test_response_content_set_and_get() {
        let rc = ResponseContent::new();
        assert!(rc.get().is_none());

        rc.set("Hello, world!".to_string());
        assert_eq!(rc.get().as_deref(), Some("Hello, world!"));
    }

    #[test]
    fn test_response_content_shared_between_clones() {
        let rc = ResponseContent::new();
        let clone = rc.clone();

        rc.set("response text".to_string());
        assert_eq!(clone.get().as_deref(), Some("response text"));
    }

    #[test]
    fn test_response_content_overwrites() {
        let rc = ResponseContent::new();
        rc.set("first".to_string());
        rc.set("second".to_string());
        assert_eq!(rc.get().as_deref(), Some("second"));
    }

    #[test]
    fn test_with_scratchpad_budget_none_matches_new() {
        let (hook_a, _, _) = StreamingRequestHook::new(Duration::from_secs(60), "req_a");
        let (hook_b, _, _) =
            StreamingRequestHook::with_scratchpad_budget(Duration::from_secs(60), "req_b", None);
        // Both hooks should report no scratchpad budget.
        assert!(hook_a.scratchpad_budget.is_none());
        assert!(hook_b.scratchpad_budget.is_none());
    }

    #[test]
    fn test_with_scratchpad_budget_stores_budget() {
        use crate::scratchpad::TiktokenCounter;
        let counter = Arc::new(TiktokenCounter::default_counter());
        let budget = ContextBudget::new(128_000, 0.20, 0, counter);
        let (hook, _, _) = StreamingRequestHook::with_scratchpad_budget(
            Duration::from_secs(60),
            "req_with_budget",
            Some(budget.clone()),
        );
        let stored = hook
            .scratchpad_budget
            .as_ref()
            .expect("budget must be stored");
        // Mutate the original; the hook's clone shares atomics so it sees the change.
        budget.record_intercepted(123);
        let (intercepted, _) = stored.scratchpad_usage();
        assert_eq!(
            intercepted, 123,
            "hook's budget must share state with caller's"
        );
    }

    #[test]
    fn test_usage_state_zero_tokens() {
        // Verify zero-token responses are handled correctly
        let usage_state = UsageState::new();

        usage_state.store_usage(0, 0, 0, false);

        let (prompt, completion, total) = usage_state.get_final_usage();
        assert_eq!(prompt, 0);
        assert_eq!(completion, 0);
        assert_eq!(total, 0);
        assert_eq!(usage_state.get_tool_completion_tokens(), 0);
    }

    #[test]
    fn test_multi_turn_context_window_tracking() {
        // Simulates a multi-turn tool call scenario within a SINGLE request:
        // Turn 1: LLM decides to call a tool (streams to frontend)
        // Turn 2: LLM calls another tool (streams to frontend)
        // Turn 3: Final text response (streams to frontend)
        //
        // All completions stream to frontend and go into thread history,
        // so we accumulate ALL completion tokens.
        let usage_state = UsageState::new();

        // Turn 1: LLM receives initial request, decides to call a tool
        // prompt=5000 (history + system + user), completion=50 (tool call JSON)
        usage_state.store_usage(5000, 50, 5050, true);

        let (prompt, completion, total) = usage_state.get_final_usage();
        assert_eq!(prompt, 5000, "First turn captures initial prompt");
        assert_eq!(completion, 50, "Completion accumulated: 50");
        assert_eq!(total, 5050, "Total = initial_prompt + accumulated");
        assert_eq!(
            usage_state.get_tool_completion_tokens(),
            50,
            "Tool completion: 50"
        );

        // Turn 2: LLM receives tool result, calls another tool
        // prompt=7000 (inflated internally), completion=60 (another tool call)
        usage_state.store_usage(7000, 60, 7060, true);

        let (prompt, completion, total) = usage_state.get_final_usage();
        assert_eq!(prompt, 5000, "Initial prompt unchanged");
        assert_eq!(completion, 110, "Completion accumulated: 50 + 60");
        assert_eq!(total, 5110, "Total = 5000 + 110");
        assert_eq!(
            usage_state.get_tool_completion_tokens(),
            110,
            "Tool completion: 50 + 60"
        );

        // Turn 3: Final response (no more tool calls)
        // prompt=9000 (inflated internally), completion=200 (final text)
        usage_state.store_usage(9000, 200, 9200, false);

        let (prompt, completion, total) = usage_state.get_final_usage();
        assert_eq!(prompt, 5000, "Initial prompt still unchanged");
        assert_eq!(completion, 310, "Completion accumulated: 50 + 60 + 200");
        assert_eq!(
            total, 5310,
            "Context window = initial_prompt(5000) + all_completions(310)"
        );
        assert_eq!(
            usage_state.get_tool_completion_tokens(),
            110,
            "Tool completion unchanged after final response turn"
        );
    }
}
