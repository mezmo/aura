//! Turn-limit nudging — warns an agent that it is approaching its turn-depth
//! limit so it wraps up and submits results instead of losing all work to a
//! `MaxDepthError`.
//!
//! # How it works
//!
//! Rig's multi-turn loop lives inside the rig fork, so aura cannot inject a
//! message between turns directly. Instead the nudge rides on two aura-owned
//! seams:
//!
//! 1. **Turn counting** — every rig turn ends with a provider `Final` chunk,
//!    which `map_stream_item` converts to `StreamItem::TurnUsage`. The
//!    `Agent::stream_*` methods count those into [`TurnNudgeState`]
//!    (see `Agent::count_turns`).
//! 2. **Injection** — [`TurnNudgeWrapper`] appends a notice to MCP tool
//!    output when few turns remain. Rig feeds tool results back as the next
//!    turn's prompt, so the model reads the nudge at the start of its next
//!    turn.
//!
//! Enabled per agent via `[agent].nudge_last_turn` (final-turn "submit now"
//! notice) and `[agent].nudge_turns_remaining` (start "wrap up" notices when
//! that many turns remain). Wired for orchestration workers (in
//! `create_worker`) and single-agent mode (in `Agent::new`); the coordinator
//! is excluded.
//!
//! # Prototype limitations
//!
//! - Only MCP tool outputs carry the nudge (`tool_wrapper` doesn't wrap
//!   native tools like scratchpad reads or `submit_result`). A turn that
//!   calls no MCP tool gets no nudge — but a turn with no tool calls at all
//!   ends the loop successfully anyway.
//! - The state is per-`Agent`; concurrent streams on one `Agent` would share
//!   a counter. All current callers build agents per request/task.
//! - Ollama fallback tool parsing executes tools outside rig's loop and is
//!   not nudged.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp_response::CallOutcome;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper, TransformOutputResult};

/// Shared turn-limit tracking for one agent stream.
///
/// Turn math: rig's streaming loop (`rig-core` `prompt_request/streaming.rs`)
/// breaks only when its pre-increment turn counter exceeds `max_depth + 1`,
/// so it executes up to `max_depth + 2` turns before yielding
/// `MaxDepthError`. A nudge appended to turn `T`'s tool output is read by
/// the model at the start of turn `T + 1`.
pub struct TurnNudgeState {
    /// Total turns rig will execute for this agent before `MaxDepthError`.
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
    /// Build nudge state from the `[agent]` flags and the agent's resolved
    /// turn depth. Returns `None` when both flags are off (nudging disabled).
    pub fn new(
        nudge_last_turn: bool,
        nudge_turns_remaining: Option<usize>,
        max_depth: usize,
    ) -> Option<Arc<Self>> {
        Self::build(nudge_last_turn, nudge_turns_remaining, max_depth, false)
    }

    /// Like [`Self::new`] but for agents that carry the orchestration
    /// `submit_result` tool — the final-turn nudge tells the agent to call
    /// it instead of answering in text.
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
        Some(Arc::new(Self {
            max_turns: max_depth + 2,
            nudge_last_turn,
            wrap_up_threshold: nudge_turns_remaining,
            has_submit_tool,
            turns_completed: AtomicUsize::new(0),
            last_nudged_turn: AtomicUsize::new(0),
        }))
    }

    /// Reset counters at stream start (an `Agent` can serve multiple
    /// sequential streams, e.g. the CLI REPL).
    pub fn reset(&self) {
        self.turns_completed.store(0, Ordering::Release);
        self.last_nudged_turn.store(0, Ordering::Release);
    }

    /// Record one completed rig turn (one `StreamItem::TurnUsage` observed).
    pub fn record_turn_completed(&self) {
        self.turns_completed.fetch_add(1, Ordering::AcqRel);
    }

    /// Nudge text for the turn currently executing, or `None` when no nudge
    /// applies (or one was already issued this turn).
    ///
    /// Called from tool-output transformation, i.e. mid-turn: the current
    /// turn number is `turns_completed + 1`, and `remaining` counts the
    /// turns left *after* this one — the turns in which the model can still
    /// act on the nudge.
    pub fn nudge_message(&self) -> Option<String> {
        let current_turn = self.turns_completed.load(Ordering::Acquire) + 1;
        let remaining = self.max_turns.saturating_sub(current_turn);
        if remaining == 0 {
            // The loop is out of turns; a nudge could no longer be acted on.
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

        // At most one nudge per turn: swap is safe because turn numbers are
        // monotonically increasing within a stream.
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

/// ToolWrapper that appends turn-limit nudges to tool output.
///
/// Composed so its `transform_output` runs last (first in the wrapper list),
/// after scratchpad interception — the nudge must land on the text the LLM
/// actually sees, and must not be persisted as raw tool output.
pub struct TurnNudgeWrapper {
    state: Arc<TurnNudgeState>,
}

impl TurnNudgeWrapper {
    pub fn new(state: Arc<TurnNudgeState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ToolWrapper for TurnNudgeWrapper {
    fn transform_output(
        &self,
        output: String,
        _outcome: &CallOutcome,
        ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> TransformOutputResult {
        match self.state.nudge_message() {
            Some(nudge) => {
                tracing::debug!(tool = %ctx.tool_name, "appending turn-limit nudge to tool output");
                TransformOutputResult::new(format!("{output}{nudge}"))
            }
            None => TransformOutputResult::new(output),
        }
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
}
