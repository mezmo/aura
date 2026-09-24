//! The split between a prepared agent and the runs it serves.
//!
//! A [`PreparedAgent`](crate::PreparedAgent) is built once from
//! configuration: it owns the provider client, the tools discovered at build
//! time, and the open MCP connections. Each turn of work is an
//! [`Agent`](crate::Agent), which owns the state belonging to that run alone
//! and drops it when the run ends. Tools are built with the prepared agent,
//! so they reach the current run's state through a [`RunSlot`] rather than
//! through a field of their own.
//!
//! # One run at a time
//!
//! Rig spawns an agent's tool server once, when the agent is built, so every
//! tool call for a prepared agent arrives on one long-lived task. Two runs
//! sharing a prepared agent would have no way to tell their tool calls apart:
//! a task-local scope does not survive that spawn, and a single slot would
//! race. The slot therefore binds exactly one run, and
//! [`PreparedAgent::begin_run`](crate::PreparedAgent::begin_run) refuses a
//! second while the first is alive. The failure it guards against is silent
//! (tool events attributed to the wrong run, no error), which is why the
//! rule is enforced rather than documented. Concurrency belongs above this
//! layer: one prepared agent per session.

use std::sync::{Arc, PoisonError, RwLock};

use crate::scratchpad::ContextBudget;
use crate::turn_nudge::TurnNudgeState;

/// State that belongs to one run of a prepared agent.
#[derive(Default)]
pub struct RunState {
    /// The run's request id.
    pub request_id: String,
    /// The run's context budget.
    pub scratchpad_budget: Option<ContextBudget>,
    /// The run's turn-limit tracking.
    pub turn_nudge: Option<Arc<TurnNudgeState>>,
}

/// A handle to the one run a prepared agent's tools currently serve.
#[derive(Clone, Default)]
pub struct RunSlot {
    bound: Arc<RwLock<Option<Arc<RunState>>>>,
}

impl RunSlot {
    /// A slot with no run bound.
    pub fn new() -> Self {
        Self::default()
    }

    /// The run currently bound, if any.
    pub fn current(&self) -> Option<Arc<RunState>> {
        self.bound
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The bound run's request id.
    pub fn request_id(&self) -> Option<String> {
        self.current().map(|run| run.request_id.clone())
    }

    /// The bound run's request id, or empty outside a run so an approval
    /// raised then still carries a well-formed id even though nothing routes
    /// it.
    pub fn request_id_or_empty(&self) -> String {
        self.request_id().unwrap_or_default()
    }

    /// The bound run's scratchpad budget.
    pub fn scratchpad_budget(&self) -> Option<ContextBudget> {
        self.current().and_then(|run| run.scratchpad_budget.clone())
    }

    /// The bound run's turn-limit nudge counters.
    pub fn turn_nudge(&self) -> Option<Arc<TurnNudgeState>> {
        self.current().and_then(|run| run.turn_nudge.clone())
    }

    /// Bind `state` as the slot's current run.
    ///
    /// Fails while another run is bound; see the module doc for why a second
    /// run is refused rather than queued or shared.
    pub(crate) fn bind(&self, state: Arc<RunState>) -> Result<(), RunInProgress> {
        let mut bound = self.bound.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(active) = bound.as_ref() {
            return Err(RunInProgress {
                active: active.request_id.clone(),
            });
        }
        *bound = Some(state);
        Ok(())
    }

    /// Unbind `state`. A no-op when a different run is bound, so a late
    /// release can never evict a run that took the slot afterwards.
    fn release(&self, state: &Arc<RunState>) {
        let mut bound = self.bound.write().unwrap_or_else(PoisonError::into_inner);
        if bound
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, state))
        {
            *bound = None;
        }
    }

    /// A slot permanently bound to `state`, for exercising a tool or wrapper
    /// outside a prepared agent.
    #[cfg(test)]
    pub(crate) fn pinned(state: RunState) -> Self {
        let slot = Self::new();
        slot.bind(Arc::new(state))
            .expect("a fresh slot has no run bound");
        slot
    }

    /// A pinned slot whose run carries only `request_id`.
    #[cfg(test)]
    pub(crate) fn pinned_request(request_id: impl Into<String>) -> Self {
        Self::pinned(RunState {
            request_id: request_id.into(),
            ..RunState::default()
        })
    }

    /// A pinned slot whose run carries only `budget`.
    #[cfg(test)]
    pub(crate) fn pinned_budget(budget: ContextBudget) -> Self {
        Self::pinned(RunState {
            scratchpad_budget: Some(budget),
            ..RunState::default()
        })
    }

    /// A pinned slot whose run carries only `turn_nudge`.
    #[cfg(test)]
    pub(crate) fn pinned_nudge(turn_nudge: Arc<TurnNudgeState>) -> Self {
        Self::pinned(RunState {
            turn_nudge: Some(turn_nudge),
            ..RunState::default()
        })
    }
}

impl std::fmt::Debug for RunSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunSlot")
            .field("request_id", &self.request_id())
            .finish()
    }
}

/// A run bound into its prepared agent's slot.
pub struct BoundRun {
    slot: RunSlot,
    state: Arc<RunState>,
}

impl std::fmt::Debug for BoundRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundRun")
            .field("request_id", &self.state.request_id)
            .finish()
    }
}

impl BoundRun {
    /// Bind `state` into `slot` for as long as the returned value lives.
    pub(crate) fn bind(slot: RunSlot, state: RunState) -> Result<Self, RunInProgress> {
        let state = Arc::new(state);
        slot.bind(Arc::clone(&state))?;
        Ok(Self { slot, state })
    }

    /// The run's state.
    pub fn state(&self) -> &RunState {
        &self.state
    }
}

impl Drop for BoundRun {
    /// Frees the slot. Every handle to a run shares one `BoundRun`, so this
    /// happens when the last of them (the `Agent` or a stream it produced)
    /// goes away.
    fn drop(&mut self) {
        self.slot.release(&self.state);
    }
}

/// A prepared agent was asked to begin a run while it still serves another.
#[derive(Debug, thiserror::Error)]
#[error("prepared agent already serves request `{active}`; it runs one request at a time")]
pub struct RunInProgress {
    /// Request id of the run holding the slot.
    pub active: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(request_id: &str) -> RunState {
        RunState {
            request_id: request_id.into(),
            ..RunState::default()
        }
    }

    #[test]
    fn an_unbound_slot_resolves_nothing() {
        let slot = RunSlot::new();
        assert!(slot.current().is_none());
        assert!(slot.request_id().is_none());
        assert!(slot.scratchpad_budget().is_none());
        assert!(slot.turn_nudge().is_none());
    }

    #[test]
    fn a_bound_run_is_visible_through_every_clone_of_the_slot() {
        let slot = RunSlot::new();
        let tool_side = slot.clone();
        let bound = BoundRun::bind(slot, run("req_a")).unwrap();

        assert_eq!(tool_side.request_id().as_deref(), Some("req_a"));
        assert_eq!(bound.state().request_id, "req_a");
    }

    #[test]
    fn a_second_run_is_refused_while_the_first_is_bound() {
        let slot = RunSlot::new();
        let first = BoundRun::bind(slot.clone(), run("req_a")).unwrap();

        let refused = BoundRun::bind(slot.clone(), run("req_b")).expect_err("slot is taken");
        assert_eq!(refused.active, "req_a");
        assert_eq!(
            slot.request_id().as_deref(),
            Some("req_a"),
            "a refused bind must not disturb the bound run",
        );

        drop(first);
        let second = BoundRun::bind(slot.clone(), run("req_b")).expect("slot is free again");
        assert_eq!(slot.request_id().as_deref(), Some("req_b"));
        drop(second);
        assert!(slot.current().is_none());
    }

    /// A handle released after the slot moved on must not evict the newer run.
    #[test]
    fn releasing_a_stale_run_leaves_a_newer_run_bound() {
        let slot = RunSlot::new();
        let stale = Arc::new(run("req_stale"));
        slot.bind(Arc::clone(&stale)).unwrap();
        slot.release(&stale);

        let current = BoundRun::bind(slot.clone(), run("req_current")).unwrap();
        slot.release(&stale);
        assert_eq!(slot.request_id().as_deref(), Some("req_current"));
        drop(current);
    }

    #[test]
    fn run_state_reaches_tools_through_the_slot() {
        use crate::scratchpad::TiktokenCounter;

        let budget =
            ContextBudget::new(1_000, 0.0, 0, Arc::new(TiktokenCounter::default_counter()));
        let nudge = TurnNudgeState::new(true, None, 2).unwrap();
        let slot = RunSlot::new();
        let bound = BoundRun::bind(
            slot.clone(),
            RunState {
                request_id: "req_a".into(),
                scratchpad_budget: Some(budget.clone()),
                turn_nudge: Some(Arc::clone(&nudge)),
            },
        )
        .unwrap();

        slot.scratchpad_budget().unwrap().record_intercepted(7);
        assert_eq!(
            budget.scratchpad_usage().0,
            7,
            "the slot hands out the run's own budget, counters shared",
        );
        assert!(Arc::ptr_eq(&slot.turn_nudge().unwrap(), &nudge));
        drop(bound);
        assert!(slot.scratchpad_budget().is_none());
    }
}
