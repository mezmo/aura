//! Turn-limit nudging — warns an agent nearing its turn-depth limit so it
//! wraps up and submits results instead of losing all work to a
//! `MaxDepthError`.
//!
//! `Agent::count_turns` tracks the turn number (one `StreamItem::TurnUsage`
//! per rig turn); [`TurnNudgeWrapper`] appends a notice to MCP tool output
//! and [`NudgedTool`] to scratchpad read tool output, which rig feeds back
//! as the next turn's prompt. Both reach the counters of the run they serve
//! through a [`RunSlot`]. Enabled via `[agent].nudge_last_turn` and
//! `[agent].nudge_turns_remaining`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::CallOutcome;
use crate::run::RunSlot;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper, TransformOutputResult};

/// Turn-limit tracking for one run of an agent.
pub struct TurnNudgeState {
    /// Total turns rig will execute before `MaxDepthError`.
    max_turns: usize,
    nudge_last_turn: bool,
    /// Emit wrap-up notices once this many turns (or fewer) remain.
    wrap_up_threshold: Option<usize>,
    /// The agent has the orchestration `submit_result` tool.
    has_submit_tool: bool,
    turns_completed: AtomicUsize,
    /// Turn number of the most recent nudge.
    last_nudged_turn: AtomicUsize,
}

impl TurnNudgeState {
    /// Returns `None` when both flags are off (nudging disabled).
    pub fn new(
        nudge_last_turn: bool,
        nudge_turns_remaining: Option<usize>,
        max_depth: usize,
    ) -> Option<Arc<Self>> {
        Self::build(nudge_last_turn, nudge_turns_remaining, max_depth, false)
    }

    /// Like [`Self::new`] but with `submit_result` wording for workers.
    pub fn new_with_submit_tool(
        nudge_last_turn: bool,
        nudge_turns_remaining: Option<usize>,
        max_depth: usize,
    ) -> Option<Arc<Self>> {
        Self::build(nudge_last_turn, nudge_turns_remaining, max_depth, true)
    }

    fn build(
        nudge_last_turn: bool,
        nudge_turns_remaining: Option<usize>,
        max_depth: usize,
        has_submit_tool: bool,
    ) -> Option<Arc<Self>> {
        if !nudge_last_turn && nudge_turns_remaining.is_none() {
            return None;
        }
        // Rig's streaming loop breaks when its pre-increment counter
        // exceeds max_depth + 1, i.e. it runs max_depth + 2 turns.
        Some(Self::with_limits(
            max_depth + 2,
            nudge_last_turn,
            nudge_turns_remaining,
            has_submit_tool,
        ))
    }

    /// Tracking against these limits with no turns completed.
    fn with_limits(
        max_turns: usize,
        nudge_last_turn: bool,
        wrap_up_threshold: Option<usize>,
        has_submit_tool: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            max_turns,
            nudge_last_turn,
            wrap_up_threshold,
            has_submit_tool,
            turns_completed: AtomicUsize::new(0),
            last_nudged_turn: AtomicUsize::new(0),
        })
    }

    /// Tracking with the same limits and no turns completed.
    pub fn fresh(&self) -> Arc<Self> {
        Self::with_limits(
            self.max_turns,
            self.nudge_last_turn,
            self.wrap_up_threshold,
            self.has_submit_tool,
        )
    }

    /// Reset counters at stream start.
    pub fn reset(&self) {
        self.turns_completed.store(0, Ordering::Release);
        self.last_nudged_turn.store(0, Ordering::Release);
    }

    /// Record one completed rig turn.
    pub fn record_turn_completed(&self) {
        self.turns_completed.fetch_add(1, Ordering::AcqRel);
    }

    /// Nudge text for the turn currently executing, or `None` when no nudge
    /// applies (or one was already issued this turn).
    pub fn nudge_message(&self) -> Option<String> {
        // Called mid-turn, so the current turn is turns_completed + 1.
        // `remaining` counts turns left after this one — the nudge is read
        // at the start of the next turn.
        let current_turn = self.turns_completed.load(Ordering::Acquire) + 1;
        let remaining = self.max_turns.saturating_sub(current_turn);
        if remaining == 0 {
            return None;
        }

        let message = if remaining == 1 {
            if self.nudge_last_turn {
                Some(self.last_turn_message())
            } else if self.wrap_up_threshold.is_some_and(|n| n >= 1) {
                Some(self.wrap_up_message(remaining))
            } else {
                None
            }
        } else if self.wrap_up_threshold.is_some_and(|n| remaining <= n) {
            Some(self.wrap_up_message(remaining))
        } else {
            None
        }?;

        // At most one nudge per turn.
        let previously_nudged = self.last_nudged_turn.swap(current_turn, Ordering::AcqRel);
        if previously_nudged == current_turn {
            return None;
        }

        tracing::info!(
            current_turn,
            remaining,
            max_turns = self.max_turns,
            "turn-limit nudge issued"
        );
        Some(message)
    }

    fn last_turn_message(&self) -> String {
        let submit = if self.has_submit_tool {
            "Call the `submit_result` tool NOW with your findings — do not call any other tools first"
        } else {
            "Respond with your final answer now — do not call any more tools"
        };
        format!(
            "\n\n---\n[TURN LIMIT — FINAL TURN] Your next turn is the LAST one before \
             this task is terminated and all work is lost. {submit}. Partial results \
             are better than none."
        )
    }

    fn wrap_up_message(&self, remaining: usize) -> String {
        let submit = if self.has_submit_tool {
            "submit your findings via the `submit_result` tool"
        } else {
            "deliver your final answer"
        };
        format!(
            "\n\n---\n[TURN LIMIT WARNING] Only {remaining} turn(s) remain before this \
             task is forcibly terminated. Start wrapping up: make only the most \
             essential remaining tool calls, then {submit} before the limit is reached."
        )
    }
}

/// Append the bound run's nudge, if one is due, to `tool`'s output. The
/// output passes through untouched when no run is bound or the bound run has
/// nudging off.
fn append_nudge(run: &RunSlot, tool: &str, output: String) -> String {
    match run.turn_nudge().and_then(|state| state.nudge_message()) {
        Some(nudge) => {
            tracing::debug!(tool, "appending turn-limit nudge to tool output");
            format!("{output}{nudge}")
        }
        None => output,
    }
}

/// ToolWrapper that appends turn-limit nudges to tool output.
pub struct TurnNudgeWrapper {
    /// The prepared agent's run slot.
    run: RunSlot,
}

impl TurnNudgeWrapper {
    pub fn new(run: RunSlot) -> Self {
        Self { run }
    }
}

#[async_trait]
impl ToolWrapper for TurnNudgeWrapper {
    async fn transform_output(
        &self,
        output: String,
        _outcome: &CallOutcome,
        ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> TransformOutputResult {
        TransformOutputResult::new(append_nudge(&self.run, &ctx.tool_name, output))
    }
}

/// Adapter that appends turn-limit nudges to a built-in tool's output —
/// the counterpart of [`TurnNudgeWrapper`] for tools (scratchpad reads)
/// whose typed args/errors don't fit `WrappedTool`.
#[derive(Clone)]
pub struct NudgedTool<T> {
    inner: T,
    /// The prepared agent's run slot.
    run: RunSlot,
}

impl<T> NudgedTool<T> {
    pub fn new(inner: T, run: RunSlot) -> Self {
        Self { inner, run }
    }
}

impl<T> rig::tool::Tool for NudgedTool<T>
where
    T: rig::tool::Tool<Output = String>,
{
    const NAME: &'static str = T::NAME;
    type Error = T::Error;
    type Args = T::Args;
    type Output = String;

    fn name(&self) -> String {
        self.inner.name()
    }

    async fn definition(&self, prompt: String) -> rig::completion::ToolDefinition {
        self.inner.definition(prompt).await
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let output = self.inner.call(args).await?;
        Ok(append_nudge(&self.run, &self.inner.name(), output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advance(state: &TurnNudgeState, turns: usize) {
        for _ in 0..turns {
            state.record_turn_completed();
        }
    }

    #[test]
    fn disabled_when_both_flags_off() {
        assert!(TurnNudgeState::new(false, None, 10).is_none());
    }

    #[test]
    fn last_turn_nudge_fires_only_on_penultimate_turn() {
        // max_depth=3 → rig executes up to 5 turns. The nudge written during
        // turn 4 is read at the start of turn 5 (the last).
        let state = TurnNudgeState::new(true, None, 3).unwrap();

        // Turns 1-3: no nudge.
        for turn in 1..=3 {
            assert!(
                state.nudge_message().is_none(),
                "unexpected nudge on turn {turn}"
            );
            state.record_turn_completed();
        }
        // Turn 4 (remaining = 1): final-turn nudge.
        let msg = state.nudge_message().expect("expected final-turn nudge");
        assert!(msg.contains("FINAL TURN"));
        // Second tool call in the same turn: no repeat.
        assert!(state.nudge_message().is_none());

        // Turn 5 (remaining = 0): too late to act, no nudge.
        state.record_turn_completed();
        assert!(state.nudge_message().is_none());
    }

    #[test]
    fn wrap_up_nudges_start_at_threshold_and_repeat_each_turn() {
        // max_depth=4 → 6 turns total. Threshold 3 → nudges when remaining
        // is 3, 2, and 1 (turns 3, 4, 5).
        let state = TurnNudgeState::new(false, Some(3), 4).unwrap();

        advance(&state, 1); // now in turn 2, remaining 4
        assert!(state.nudge_message().is_none());

        advance(&state, 1); // turn 3, remaining 3
        let msg = state.nudge_message().expect("expected wrap-up nudge");
        assert!(msg.contains("Only 3 turn(s) remain"));

        advance(&state, 1); // turn 4, remaining 2
        assert!(
            state
                .nudge_message()
                .expect("expected wrap-up nudge")
                .contains("Only 2 turn(s) remain")
        );

        advance(&state, 1); // turn 5, remaining 1 — wrap-up wording (last-turn flag off)
        assert!(
            state
                .nudge_message()
                .expect("expected wrap-up nudge")
                .contains("Only 1 turn(s) remain")
        );
    }

    #[test]
    fn last_turn_message_wins_over_wrap_up_on_penultimate_turn() {
        let state = TurnNudgeState::new(true, Some(2), 2).unwrap(); // 4 turns total
        advance(&state, 2); // turn 3, remaining 1 — both flags apply
        let msg = state.nudge_message().expect("expected nudge");
        assert!(msg.contains("FINAL TURN"));
    }

    #[test]
    fn submit_tool_wording_for_workers() {
        let state = TurnNudgeState::new_with_submit_tool(true, Some(2), 2).unwrap();
        advance(&state, 1); // turn 2, remaining 2 → wrap-up
        assert!(
            state
                .nudge_message()
                .expect("expected wrap-up nudge")
                .contains("submit_result")
        );
        advance(&state, 1); // turn 3, remaining 1 → final
        assert!(
            state
                .nudge_message()
                .expect("expected final nudge")
                .contains("submit_result")
        );
    }

    #[test]
    fn reset_clears_counters_between_streams() {
        let state = TurnNudgeState::new(true, None, 1).unwrap(); // 3 turns total
        advance(&state, 1); // turn 2, remaining 1
        assert!(state.nudge_message().is_some());
        state.reset();
        // Back in turn 1 (remaining 2): no nudge.
        assert!(state.nudge_message().is_none());
    }

    #[test]
    fn fresh_keeps_the_limits_and_starts_the_count_over() {
        let seed = TurnNudgeState::new(true, None, 1).unwrap(); // 3 turns total
        advance(&seed, 1); // turn 2, remaining 1
        assert!(seed.nudge_message().is_some());

        let run = seed.fresh();
        assert!(
            run.nudge_message().is_none(),
            "a fresh run is back in turn 1"
        );
        advance(&run, 1);
        assert!(
            run.nudge_message().is_some(),
            "the fresh run nudges at the same limit as the seed",
        );
        assert_eq!(
            seed.turns_completed.load(Ordering::Acquire),
            1,
            "advancing the fresh run leaves the seed untouched",
        );
    }

    #[derive(Clone)]
    struct EchoTool;

    impl rig::tool::Tool for EchoTool {
        const NAME: &'static str = "echo";
        type Error = std::io::Error;
        type Args = String;
        type Output = String;

        async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
            rig::completion::ToolDefinition {
                name: Self::NAME.to_string(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
            Ok(args)
        }
    }

    #[tokio::test]
    async fn nudged_tool_appends_nudge_near_limit() {
        use rig::tool::Tool;

        let state = TurnNudgeState::new(true, None, 1).unwrap(); // 3 turns total
        let tool = NudgedTool::new(EchoTool, RunSlot::pinned_nudge(state.clone()));

        // Turn 1 (remaining 2): output passes through untouched.
        let out = tool.call("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");

        // Turn 2 (remaining 1): final-turn nudge appended.
        state.record_turn_completed();
        let out = tool.call("hello".to_string()).await.unwrap();
        assert!(out.starts_with("hello"));
        assert!(out.contains("FINAL TURN"));

        // Second call in the same turn: shared state dedupes the nudge.
        let out = tool.call("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn nudged_tool_without_a_run_is_passthrough() {
        use rig::tool::Tool;

        let tool = NudgedTool::new(EchoTool, RunSlot::new());
        assert_eq!(tool.name(), "echo");
        let out = tool.call("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    /// The wrapper and tool hold the slot, not the counters, so the nudge
    /// follows whichever run is bound when the call happens.
    #[tokio::test]
    async fn nudged_tool_follows_the_run_bound_at_call_time() {
        use crate::run::{BoundRun, RunState};
        use rig::tool::Tool;

        let slot = RunSlot::new();
        let tool = NudgedTool::new(EchoTool, slot.clone());
        let seed = TurnNudgeState::new(true, None, 1).unwrap(); // 3 turns total

        let first = BoundRun::bind(
            slot.clone(),
            RunState {
                turn_nudge: Some(seed.fresh()),
                ..RunState::default()
            },
        )
        .unwrap();
        first
            .state()
            .turn_nudge
            .as_ref()
            .unwrap()
            .record_turn_completed();
        assert!(
            tool.call("hello".to_string())
                .await
                .unwrap()
                .contains("FINAL TURN"),
            "the first run is on its penultimate turn",
        );
        drop(first);

        let _second = BoundRun::bind(
            slot.clone(),
            RunState {
                turn_nudge: Some(seed.fresh()),
                ..RunState::default()
            },
        )
        .unwrap();
        assert_eq!(
            tool.call("hello".to_string()).await.unwrap(),
            "hello",
            "the second run starts its count over",
        );
    }
}
