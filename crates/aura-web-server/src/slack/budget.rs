//! A count of tool calls one run may still make, shared by every tool
//! instance built for that run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Calls left for one run.
pub struct Budget(AtomicUsize);

impl Budget {
    pub fn new(calls: usize) -> Arc<Self> {
        Arc::new(Self(AtomicUsize::new(calls)))
    }

    /// Spend one call; `false` when none are left. Several tools built for
    /// the same run share one budget, so a worker that has not called yet
    /// still finds it spent once its siblings have.
    pub fn take(&self) -> bool {
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_spends_down_to_zero_once() {
        let budget = Budget::new(2);
        assert!(budget.take());
        assert!(budget.take());
        assert!(!budget.take());
        assert!(!budget.take());
    }
}
