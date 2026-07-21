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
    /// generation.
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
    /// The presented arguments differ from the ones the human approved. The
    /// call is denied; the decision stays `Resolved` - it never applied to
    /// those arguments.
    DigestMismatch {
        bound: ArgsDigest,
        presented: ArgsDigest,
    },
}

impl DecisionDispatchState {
    /// Apply one event, consuming the current state. `bound` is the digest
    /// recorded when the approval parked.
    ///
    /// Legality: `Pending` only resolves; `Resolved` is claimed by exactly
    /// one dispatcher, and only with a digest equal to `bound`;
    /// `DispatchClaimed` confirms `Executed` or degrades to
    /// `ExecutionUnknown`; `Executed` and `ExecutionUnknown` absorb nothing.
    pub fn apply(self, event: DispatchEvent, bound: &ArgsDigest) -> Result<Self, DispatchError> {
        let _ = (event, bound);
        todo!("staged for #271 P-cards: dispatch consumption FSM")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn bound() -> ArgsDigest {
        ArgsDigest::test_value("digest-bound")
    }

    fn resolved() -> DecisionDispatchState {
        DecisionDispatchState::Resolved {
            decision: ApprovalDecision::Approved,
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
    fn resolved_claims_with_matching_digest() {
        let next = resolved().apply(claim(bound()), &bound()).expect("legal");
        assert!(matches!(
            next,
            DecisionDispatchState::DispatchClaimed { .. }
        ));
    }

    #[test]
    fn digest_mismatch_denies_and_preserves_binding() {
        let presented = ArgsDigest::test_value("digest-tampered");
        assert_eq!(
            resolved().apply(claim(presented.clone()), &bound()),
            Err(DispatchError::DigestMismatch {
                bound: bound(),
                presented,
            })
        );
    }

    #[test]
    fn claimed_dispatcher_loss_is_execution_unknown() {
        let claimed = resolved().apply(claim(bound()), &bound()).expect("legal");
        let next = claimed
            .apply(DispatchEvent::LoseDispatcher { at: Utc::now() }, &bound())
            .expect("legal");
        assert!(matches!(
            next,
            DecisionDispatchState::ExecutionUnknown { .. }
        ));
    }

    #[test]
    fn executed_is_terminal() {
        let executed = DecisionDispatchState::Executed {
            executed_at: Utc::now(),
        };
        assert!(matches!(
            executed.apply(claim(bound()), &bound()),
            Err(DispatchError::Illegal { .. })
        ));
    }

    #[test]
    fn resolved_cannot_resolve_twice() {
        assert!(matches!(
            resolved().apply(
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
