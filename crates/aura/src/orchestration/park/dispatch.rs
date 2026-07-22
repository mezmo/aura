//! Approval dispatch consumption FSM (ADR 2026-07-21, decision 9).
//!
//! This is the consumption phase of an *approved* decision, not the approval
//! lifecycle. Resolution (pending -> approved/denied) belongs to the HITL
//! layer and to the durable wake reason (decision 8). A dispatch record
//! exists only once an approval is granted, so a denial has no state here at
//! all and cannot reach execution by construction.

use serde::{Deserialize, Serialize};

use crate::hitl::Timestamp;

use super::lease::FencingGeneration;

/// SHA-256 hex digest over the RFC 8785 (JCS) canonical form of a tool
/// call's arguments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArgsDigest(String);

impl ArgsDigest {
    /// Canonicalize (RFC 8785) and hash the arguments.
    pub fn compute(args: &serde_json::Value) -> Self {
        let _ = args;
        todo!("staged for #271 P-cards: JCS canonicalization + SHA-256")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn test_value(raw: &str) -> Self {
        Self(raw.to_string())
    }
}

/// Where one parked approval's decision stands. [`DispatchState`] - the
/// consumption of a granted call - exists only inside `Approved`, so a
/// pending or denied decision has no dispatch record: "a denial never
/// reaches execution" holds because no value of any type can construct one.
/// `Pending` and `Denied` are distinct variants, never conflated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum DecisionConsumption {
    /// No human decision yet.
    Pending,
    /// The human denied; terminal, never dispatched.
    Denied {
        reason: Option<String>,
        at: Timestamp,
    },
    /// The human approved; consumption proceeds through `dispatch`.
    Approved { dispatch: DispatchState },
}

/// Consumption of a granted approval. Reachable only through
/// [`DecisionConsumption::Approved`], so no value of this type can stand
/// for a denied or undecided approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum DispatchState {
    /// The approval is granted; no dispatcher has claimed it.
    Unclaimed,
    /// One dispatcher claimed it for execution under its fencing generation.
    Claimed {
        generation: FencingGeneration,
        claimed_at: Timestamp,
    },
    /// The gated call ran to a known result.
    Executed { executed_at: Timestamp },
    /// The dispatcher died after claiming; whether the call ran is unknown.
    ExecutionUnknown { claimed_at: Timestamp },
}

/// Events the dispatch FSM accepts. Resolution is not among them: it is the
/// approval's concern, upstream of any dispatch record.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchEvent {
    /// Claim the decision for execution, presenting the digest of the
    /// arguments about to run.
    Claim {
        generation: FencingGeneration,
        presented: ArgsDigest,
        at: Timestamp,
    },
    ConfirmExecuted {
        at: Timestamp,
    },
    /// The claiming dispatcher is gone (lease expiry, crash recovery).
    LoseDispatcher {
        at: Timestamp,
    },
}

/// A rejected dispatch transition.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchError {
    Illegal {
        from: DispatchState,
        event: DispatchEvent,
    },
    /// The presented arguments differ from the ones the human approved.
    DigestMismatch {
        bound: ArgsDigest,
        presented: ArgsDigest,
    },
}

impl DispatchState {
    /// Apply one event, returning the next state and leaving `self`
    /// untouched - a rejected event provably consumes nothing, which is
    /// what "a digest mismatch leaves the decision unconsumed" means at the
    /// type level. `bound` is the digest recorded when the approval parked.
    ///
    /// Legality: `Unclaimed` is claimed by exactly one dispatcher, and only
    /// with a digest equal to `bound`; `Claimed` confirms `Executed` or
    /// degrades to `ExecutionUnknown` but rejects a second `Claim`, so one
    /// binding is consumed by at most one dispatcher; `Executed` and
    /// `ExecutionUnknown` accept no event.
    pub fn apply(&self, event: DispatchEvent, bound: &ArgsDigest) -> Result<Self, DispatchError> {
        let _ = (event, bound);
        todo!("staged for #271 P-cards: dispatch consumption FSM")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn bound() -> ArgsDigest {
        ArgsDigest::test_value("digest-bound")
    }

    fn claim_at(generation: FencingGeneration, presented: ArgsDigest) -> DispatchEvent {
        DispatchEvent::Claim {
            generation,
            presented,
            at: Utc::now(),
        }
    }

    fn claim(presented: ArgsDigest) -> DispatchEvent {
        claim_at(FencingGeneration::INITIAL.next(), presented)
    }

    fn all_events() -> Vec<DispatchEvent> {
        vec![
            claim(bound()),
            DispatchEvent::ConfirmExecuted { at: Utc::now() },
            DispatchEvent::LoseDispatcher { at: Utc::now() },
        ]
    }

    #[test]
    fn digest_is_canonical_and_key_order_insensitive() {
        // SHA-256 of the RFC 8785 form `{"a":1,"b":2}`, pinned so a
        // non-canonical or constant implementation cannot pass.
        const VECTOR: &str = "43258cff783fe7036d8a43033f830adfc60ec037382473548ac742b888292777";
        let digest = ArgsDigest::compute(&json!({"b": 2, "a": 1}));
        assert_eq!(digest.as_str(), VECTOR);
        assert_eq!(digest, ArgsDigest::compute(&json!({"a": 1, "b": 2})));
    }

    #[test]
    fn digests_differ_for_different_arguments() {
        assert_ne!(
            ArgsDigest::compute(&json!({"a": 1})),
            ArgsDigest::compute(&json!({"a": 2}))
        );
    }

    #[test]
    fn unclaimed_claims_with_matching_digest() {
        let next = DispatchState::Unclaimed
            .apply(claim(bound()), &bound())
            .expect("legal");
        assert!(matches!(next, DispatchState::Claimed { .. }));
    }

    #[test]
    fn digest_mismatch_rejected_and_record_preserved() {
        let state = DispatchState::Unclaimed;
        let presented = ArgsDigest::test_value("digest-tampered");
        assert_eq!(
            state.apply(claim(presented.clone()), &bound()),
            Err(DispatchError::DigestMismatch {
                bound: bound(),
                presented,
            })
        );
        // The rejected claim consumed nothing: the same record still
        // dispatches for the arguments the human actually approved.
        let next = state.apply(claim(bound()), &bound()).expect("legal");
        assert!(matches!(next, DispatchState::Claimed { .. }));
    }

    #[test]
    fn unclaimed_rejects_premature_events() {
        for event in [
            DispatchEvent::ConfirmExecuted { at: Utc::now() },
            DispatchEvent::LoseDispatcher { at: Utc::now() },
        ] {
            assert!(matches!(
                DispatchState::Unclaimed.apply(event, &bound()),
                Err(DispatchError::Illegal { .. })
            ));
        }
    }

    #[test]
    fn claimed_rejects_a_second_claim() {
        let claimed = DispatchState::Unclaimed
            .apply(claim(bound()), &bound())
            .expect("legal");
        // A second dispatcher must not consume the same binding - not at the
        // same generation, and not at a later one after a lease rotation.
        let later = FencingGeneration::INITIAL.next().next();
        for second in [claim(bound()), claim_at(later, bound())] {
            assert!(matches!(
                claimed.apply(second, &bound()),
                Err(DispatchError::Illegal { .. })
            ));
        }
    }

    #[test]
    fn claimed_confirms_executed() {
        let claimed = DispatchState::Unclaimed
            .apply(claim(bound()), &bound())
            .expect("legal");
        let next = claimed
            .apply(DispatchEvent::ConfirmExecuted { at: Utc::now() }, &bound())
            .expect("legal");
        assert!(matches!(next, DispatchState::Executed { .. }));
    }

    #[test]
    fn claimed_dispatcher_loss_is_execution_unknown() {
        let claimed = DispatchState::Unclaimed
            .apply(claim(bound()), &bound())
            .expect("legal");
        let next = claimed
            .apply(DispatchEvent::LoseDispatcher { at: Utc::now() }, &bound())
            .expect("legal");
        assert!(matches!(next, DispatchState::ExecutionUnknown { .. }));
    }

    #[test]
    fn terminal_states_absorb_no_event() {
        let terminals = [
            DispatchState::Executed {
                executed_at: Utc::now(),
            },
            DispatchState::ExecutionUnknown {
                claimed_at: Utc::now(),
            },
        ];
        for terminal in terminals {
            for event in all_events() {
                assert!(
                    terminal.apply(event.clone(), &bound()).is_err(),
                    "{terminal:?} must reject {event:?}"
                );
            }
        }
    }
}
