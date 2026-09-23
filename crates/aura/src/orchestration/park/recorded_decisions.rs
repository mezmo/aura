//! The recorded-decisions set: decisions already recorded for a run's parked
//! calls, consumed at most once each at the resume gate.
//!
//! Vocabulary bounding line: the approval *request* is raised by the park arm;
//! the *decision* recorded against it is what this set holds. "Ticket" is
//! retired vocabulary (DECISIONS-2026-09-03 item 2).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::hitl::ApprovalDecision;

/// Decisions already recorded for this run's parked calls, consumed at most
/// once each. `strict_tasks` holds the task ids whose continuation is
/// mid-invocation: a miss for one of those tasks is a resume fault, not a
/// fresh park. Per task, because sibling continuations run concurrently in
/// one wave. It lives here, behind the `Arc` the gate already holds, because
/// `pre_call` runs in a spawned task (the `pre_handle` spawn in the tool
/// wrapper) and a task-local would not cross that boundary. The continuation
/// drives `set_strict` through a drop guard so every exit path (error, panic,
/// mismatch) clears the task's entry.
#[derive(Debug, Default)]
pub(crate) struct RecordedDecisions {
    entries: Mutex<HashMap<CallKey, VecDeque<ApprovalDecision>>>,
    strict_tasks: Mutex<HashSet<usize>>,
}

impl RecordedDecisions {
    /// Record a decision for a key, appending to that key's recorded-order
    /// queue so two same-turn calls with identical arguments keep their own
    /// decisions. Wired by the orchestrator continuation (P44 commit 3).
    #[allow(dead_code)]
    pub(crate) fn push(&self, key: CallKey, decision: ApprovalDecision) {
        self.entries
            .lock()
            .expect("recorded-decisions lock")
            .entry(key)
            .or_default()
            .push_back(decision);
    }

    /// Mark a task's continuation as in-flight (`on = true`) or clear it. A
    /// miss for a strict task is a resume fault; a miss otherwise re-parks.
    #[allow(dead_code)]
    pub(crate) fn set_strict(&self, task_id: usize, on: bool) {
        let mut strict = self.strict_tasks.lock().expect("recorded-decisions lock");
        if on {
            strict.insert(task_id);
        } else {
            strict.remove(&task_id);
        }
    }

    /// Whether a task's continuation is in-flight, and so a recorded-decisions
    /// miss must fail closed rather than re-park.
    pub(crate) fn is_strict(&self, task_id: usize) -> bool {
        self.strict_tasks
            .lock()
            .expect("recorded-decisions lock")
            .contains(&task_id)
    }

    /// Consume one decision for a key in recorded order; the next call returns
    /// the following decision, and an empty queue returns `None`.
    pub(crate) fn take(&self, key: &CallKey) -> Option<ApprovalDecision> {
        self.entries
            .lock()
            .expect("recorded-decisions lock")
            .get_mut(key)
            .and_then(VecDeque::pop_front)
    }

    /// Arm a drop guard that holds `task_id` strict until the guard is
    /// dropped, then clears it. Holds a clone of the `Arc` so the clear runs
    /// on every exit path (normal return, `?`-early-return, error, and panic
    /// unwind) regardless of the recorder's own lifetime. Wired by the
    /// orchestrator continuation (P44 commit 3).
    #[allow(dead_code)]
    pub(crate) fn strict_guard(self: &Arc<Self>, task_id: usize) -> StrictGuard {
        self.set_strict(task_id, true);
        StrictGuard {
            recorded: Arc::clone(self),
            task_id,
        }
    }
}

/// Key for a recorded decision: the task the call belongs to, the tool name,
/// and a digest of the canonical arguments. `serde_json` in this workspace is
/// built without `preserve_order`, so `Value::to_string` is BTreeMap-ordered
/// and canonical; a test pins that. The separator byte (`0x00`) keeps the tool
/// name and the arguments from aliasing across names that end where another
/// begins.
#[derive(Debug, Hash, PartialEq, Eq)]
pub(crate) struct CallKey {
    task_id: usize,
    tool_name: String,
    args_digest: [u8; 32],
}

impl CallKey {
    /// Build a key from the task id, the tool name, and a digest of
    /// `tool_name || 0x00 || canonical_json(args)`.
    pub(crate) fn new(task_id: usize, tool_name: &str, args: &Value) -> Self {
        let mut h = Sha256::new();
        h.update(tool_name.as_bytes());
        h.update([0u8]);
        h.update(args.to_string().as_bytes());
        Self {
            task_id,
            tool_name: tool_name.to_owned(),
            args_digest: h.finalize().into(),
        }
    }
}

/// RAII guard for a task's strict entry: clears the task from
/// `strict_tasks` on drop, so a leaked strict entry can never silently
/// convert a model-issued re-park into a resume fault. Constructed via
/// [`RecordedDecisions::strict_guard`]. Wired by the orchestrator
/// continuation (P44 commit 3).
#[allow(dead_code)]
pub(crate) struct StrictGuard {
    recorded: Arc<RecordedDecisions>,
    task_id: usize,
}

impl Drop for StrictGuard {
    fn drop(&mut self) {
        self.recorded.set_strict(self.task_id, false);
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    fn key(task_id: usize, tool_name: &str, args: &Value) -> CallKey {
        CallKey::new(task_id, tool_name, args)
    }

    /// Two same-key decisions consume in recorded order; a third take is
    /// `None` (the queue is per-key and exhausted by consumption).
    #[test]
    fn take_consumes_per_key_queue_in_recorded_order() {
        let recorded = RecordedDecisions::default();
        let args = serde_json::json!({"namespace": "prod"});

        recorded.push(key(1, "kubectl_apply", &args), ApprovalDecision::Approved);
        recorded.push(
            key(1, "kubectl_apply", &args),
            ApprovalDecision::Denied {
                reason: Some("too risky".to_string()),
            },
        );

        let probe = key(1, "kubectl_apply", &args);
        assert_eq!(
            recorded.take(&probe),
            Some(ApprovalDecision::Approved),
            "first take returns the first recorded decision",
        );
        assert_eq!(
            recorded
                .take(&probe)
                .map(|d| matches!(d, ApprovalDecision::Denied { .. })),
            Some(true),
            "second take returns the second recorded decision, in order",
        );
        assert!(
            recorded.take(&probe).is_none(),
            "third take exhausts the queue"
        );
    }

    /// `set_strict` toggles membership; `is_strict` reports it verbatim.
    #[test]
    fn strict_set_toggles_membership() {
        let recorded = RecordedDecisions::default();
        assert!(!recorded.is_strict(1));
        recorded.set_strict(1, true);
        assert!(recorded.is_strict(1), "set_strict(true) enters the task");
        assert!(
            !recorded.is_strict(2),
            "a sibling task is unaffected by another's strict entry",
        );
        recorded.set_strict(1, false);
        assert!(!recorded.is_strict(1), "set_strict(false) clears the task");
    }

    /// The drop guard clears the task's strict entry on a normal drop, so a
    /// scope exit (early return, error, end of block) cannot leak the entry.
    #[test]
    fn strict_guard_clears_on_normal_drop() {
        let recorded = std::sync::Arc::new(RecordedDecisions::default());
        {
            let _guard = recorded.strict_guard(1);
            assert!(recorded.is_strict(1), "guard arms the strict entry");
        }
        assert!(
            !recorded.is_strict(1),
            "guard's drop clears the entry after the scope ends",
        );
    }

    /// An early return out of a scope that holds the guard still drops the
    /// guard, so the strict entry is cleared.
    #[test]
    fn strict_guard_clears_on_early_return() {
        let recorded = std::sync::Arc::new(RecordedDecisions::default());

        fn drive(recorded: &std::sync::Arc<RecordedDecisions>) -> Result<(), &'static str> {
            let _guard = recorded.strict_guard(1);
            // An early-return path the continuation might take on a non-fatal
            // error: the guard drops here, not at the end of the function.
            Err("simulated non-fatal error")
        }

        assert!(drive(&recorded).is_err());
        assert!(
            !recorded.is_strict(1),
            "an early return drops the guard and clears strict",
        );
    }

    /// A panic unwinds through the guard's scope, so its `Drop` runs and
    /// clears the strict entry; the recorder is left consistent.
    #[test]
    fn strict_guard_clears_on_panic_unwind() {
        let recorded = std::sync::Arc::new(RecordedDecisions::default());
        let guard = recorded.strict_guard(1);
        assert!(recorded.is_strict(1));

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _held = guard;
            panic!("simulated continuation fault");
        }));
        assert!(result.is_err(), "the panic must be caught");
        assert!(
            !recorded.is_strict(1),
            "the guard's drop must clear strict even when unwinding",
        );
    }

    /// The same arguments in different insertion order digests to the same
    /// key: `serde_json` without `preserve_order` serializes `Value::Object`
    /// BTreeMap-ordered, so `to_string` is canonical. A test pins that.
    #[test]
    fn digest_is_stable_across_argument_key_order() {
        let a = key(1, "kubectl_apply", &serde_json::json!({"a": 1, "b": 2}));
        let b = key(1, "kubectl_apply", &serde_json::json!({"b": 2, "a": 1}));
        assert_eq!(
            a.args_digest, b.args_digest,
            "canonical json: key order must not affect the digest",
        );
        assert!(a == b, "the whole key compares equal when the digest does");
    }

    /// Different arguments produce different digests, so two distinct calls
    /// do not collide on the same key.
    #[test]
    fn different_arguments_digest_differ() {
        let a = key(
            1,
            "kubectl_apply",
            &serde_json::json!({"namespace": "prod"}),
        );
        let b = key(
            1,
            "kubectl_apply",
            &serde_json::json!({"namespace": "stage"}),
        );
        assert_ne!(a.args_digest, b.args_digest);
        assert!(a != b);
    }

    /// Different tool names produce different digests even for the same
    /// arguments: the name is part of the hashed bytes.
    #[test]
    fn different_tool_names_digest_differ() {
        let args = serde_json::json!({"namespace": "prod"});
        let a = key(1, "kubectl_apply", &args);
        let b = key(1, "kubectl_delete", &args);
        assert!(a != b);
    }
}
