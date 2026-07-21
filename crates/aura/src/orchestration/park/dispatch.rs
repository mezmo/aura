//! Approval dispatch consumption FSM (ADR 2026-07-21, decision 9).

use serde::{Deserialize, Serialize};

use crate::hitl::{ApprovalDecision, Timestamp};

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

/// Where a decision stands on its way to consumption.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "dispatch", rename_all = "snake_case")]
pub enum DecisionDispatchState {
    /// Parked; no human decision yet.
    Pending,
    /// A human decided; not yet dispatched.
    Resolved {
        decision: ApprovalDecision,
        resolved_at: Timestamp,
    },
    /// One dispatcher claimed the decision for execution under its fencing
    /// generation. Only an approval is claimable, so a claimed state always
    /// stands for an approved call.
    DispatchClaimed {
        generation: FencingGeneration,
        claimed_at: Timestamp,
    },
    /// The gated call ran to a known result.
    Executed { executed_at: Timestamp },
    /// The dispatcher died after claiming; whether the call ran is unknown.
    ExecutionUnknown { claimed_at: Timestamp },
}

/// Events the dispatch FSM accepts.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchEvent {
    Resolve {
        decision: ApprovalDecision,
        at: Timestamp,
    },
    /// Claim the decision for execution, presenting the digest of the
    /// arguments about to run.
    ClaimDispatch {
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
        from: DecisionDispatchState,
        event: DispatchEvent,
    },
    /// The presented arguments differ from the ones the human approved.
    DigestMismatch {
        bound: ArgsDigest,
        presented: ArgsDigest,
    },
}

impl DecisionDispatchState {
    /// Apply one event, returning the next state and leaving `self`
    /// untouched - a rejected event provably consumes nothing, which is
    /// what "a digest mismatch leaves the decision unconsumed" means at the
    /// type level. `bound` is the digest recorded when the approval parked.
    ///
    /// Legality: `Pending` only resolves; `Resolved { Approved }` is
    /// claimed by exactly one dispatcher, and only with a digest equal to
    /// `bound`; `Resolved { Denied }` is never claimable - a denial cannot
    /// reach execution; `DispatchClaimed` confirms `Executed` or degrades
    /// to `ExecutionUnknown`; `Executed` and `ExecutionUnknown` accept no
    /// event.
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

    fn resolved(decision: ApprovalDecision) -> DecisionDispatchState {
        DecisionDispatchState::Resolved {
            decision,
            resolved_at: Utc::now(),
        }
    }

    fn claim(presented: ArgsDigest) -> DispatchEvent {
        DispatchEvent::ClaimDispatch {
            generation: FencingGeneration::INITIAL.next(),
            presented,
            at: Utc::now(),
        }
    }

    fn all_events() -> Vec<DispatchEvent> {
        vec![
            DispatchEvent::Resolve {
                decision: ApprovalDecision::Approved,
                at: Utc::now(),
            },
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
    fn pending_resolves() {
        let next = DecisionDispatchState::Pending
            .apply(
                DispatchEvent::Resolve {
                    decision: ApprovalDecision::Approved,
                    at: Utc::now(),
                },
                &bound(),
            )
            .expect("legal");
        assert!(matches!(next, DecisionDispatchState::Resolved { .. }));
    }

    #[test]
    fn pending_cannot_be_claimed() {
        assert!(matches!(
            DecisionDispatchState::Pending.apply(claim(bound()), &bound()),
            Err(DispatchError::Illegal { .. })
        ));
    }

    #[test]
    fn approved_claims_with_matching_digest() {
        let next = resolved(ApprovalDecision::Approved)
            .apply(claim(bound()), &bound())
            .expect("legal");
        assert!(matches!(
            next,
            DecisionDispatchState::DispatchClaimed { .. }
        ));
    }

    #[test]
    fn denied_decision_cannot_be_claimed() {
        let denied = resolved(ApprovalDecision::Denied {
            reason: Some("no".to_string()),
        });
        assert!(matches!(
            denied.apply(claim(bound()), &bound()),
            Err(DispatchError::Illegal { .. })
        ));
    }

    #[test]
    fn digest_mismatch_denies_and_preserves_the_decision() {
        let state = resolved(ApprovalDecision::Approved);
        let presented = ArgsDigest::test_value("digest-tampered");
        assert_eq!(
            state.apply(claim(presented.clone()), &bound()),
            Err(DispatchError::DigestMismatch {
                bound: bound(),
                presented,
            })
        );
        // The rejected claim consumed nothing: the same decision still
        // dispatches for the arguments the human actually approved.
        let next = state.apply(claim(bound()), &bound()).expect("legal");
        assert!(matches!(
            next,
            DecisionDispatchState::DispatchClaimed { .. }
        ));
    }

    #[test]
    fn claimed_dispatcher_loss_is_execution_unknown() {
        let claimed = resolved(ApprovalDecision::Approved)
            .apply(claim(bound()), &bound())
            .expect("legal");
        let next = claimed
            .apply(DispatchEvent::LoseDispatcher { at: Utc::now() }, &bound())
            .expect("legal");
        assert!(matches!(
            next,
            DecisionDispatchState::ExecutionUnknown { .. }
        ));
    }

    #[test]
    fn terminal_dispatch_states_absorb_no_event() {
        let terminals = [
            DecisionDispatchState::Executed {
                executed_at: Utc::now(),
            },
            DecisionDispatchState::ExecutionUnknown {
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

    #[test]
    fn resolved_cannot_resolve_twice() {
        assert!(matches!(
            resolved(ApprovalDecision::Approved).apply(
                DispatchEvent::Resolve {
                    decision: ApprovalDecision::Approved,
                    at: Utc::now(),
                },
                &bound(),
            ),
            Err(DispatchError::Illegal { .. })
        ));
    }
}
