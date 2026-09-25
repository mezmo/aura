//! A run's own event stream: what its agent did and what happened to the run.
//!
//! [`AgentEvent`] says what an agent did. It says nothing about the run it
//! belongs to — that it started, who is watching, that it finished or was
//! cancelled and why. A [`RunEvent`] is the envelope that carries both, so a
//! projection reads one ordered sequence and never correlates two.
//!
//! # Correlation lives on the envelope
//!
//! The run and session ids that [`AgentEvent`] deliberately omits are here,
//! applied once by whatever owns the run rather than by every observer.
//!
//! A run has one id. Whatever starts a run mints its [`RunId`] once, and every
//! place that names the run — task-local scope, orchestration persistence, the
//! owner of a parked approval — names it by this value. The envelope never
//! carries a second id for the same run.
//!
//! # Sequence numbers are dense
//!
//! Every event of a run carries a [`SequenceNumber`], starting at
//! [`SequenceNumber::FIRST`] and increasing by exactly one per event. A
//! consumer that sees a gap knows it missed something rather than silently
//! reading a partial stream. The number is minted in one place — by whatever
//! appends to the run's stream — never by a relay or projection.
//!
//! # Lifecycle beside activity
//!
//! [`AgentEventPayload::RunParked`](crate::agent::AgentEventPayload::RunParked)
//! is an agent event: the orchestrator emits it from inside the run with its
//! iteration state. [`RunLifecycleEvent::Parked`] is the run owner's
//! acknowledgement and carries the checkpoint reference. Both appear in the
//! stream; a projection decides which it surfaces.
//!
//! [`RunLifecycleEvent::Started`] carries the prompt, not the history. History
//! is the run's input, too large to replay to every late observer.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::AgentEvent;
use crate::{string_newtype, RunId, SessionId, TokenUsage};

/// An instant as Unix time in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    pub fn from_unix_millis(millis: u64) -> Self {
        Self(millis)
    }

    pub fn unix_millis(self) -> u64 {
        self.0
    }

    /// The current instant. A clock set before the Unix epoch reads as the
    /// epoch itself.
    pub fn now() -> Self {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self(u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX))
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// An event's position in its run's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SequenceNumber(u64);

impl SequenceNumber {
    /// The number of a run's first event.
    pub const FIRST: Self = Self(1);

    pub fn get(self) -> u64 {
        self.0
    }

    /// The number the event after this one carries.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Whether this is the event immediately after `previous`. `false` means
    /// the consumer missed at least one event between them.
    pub fn follows(self, previous: Self) -> bool {
        self.0 == previous.0.saturating_add(1)
    }
}

impl std::fmt::Display for SequenceNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

string_newtype! {
    /// Identifier for one observer of a run, unique among the run's observers.
    ObserverId
}

string_newtype! {
    /// Locates a parked run's checkpoint, as the park machinery names it.
    CheckpointRef
}

/// What an observer's subscription does to the run's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverKind {
    /// Reads the stream and has no effect on the run's lifetime.
    Collecting,
    /// Holds the run's right to continue.
    Claiming,
}

/// One observer of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observer {
    pub id: ObserverId,
    pub kind: ObserverKind,
    /// Whether a human is at this observer.
    pub presence: bool,
}

/// What a run does once nothing claims it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LivenessPolicy {
    Cancel,
    /// Run to completion unclaimed.
    Continue,
    /// Checkpoint and end the run resumable.
    Park,
}

/// Why a run was ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RunCancelReason {
    /// The run outlived the bound it was started with.
    Deadline {
        #[serde(rename = "after_ms", with = "duration_ms")]
        after: Duration,
    },
    /// Something outside the run cancelled it.
    External,
    /// The model called a tool the caller executes, so the run yields to it.
    ClientTool,
}

/// What happened to a run, as distinct from what its agent did.
///
/// `#[non_exhaustive]` because the vocabulary grows as the run owner learns
/// to say more.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunLifecycleEvent {
    Started {
        /// The configured agent running, matching
        /// [`AgentInfo::id`](crate::AgentInfo::id).
        agent: String,
        prompt: String,
        #[serde(
            rename = "timeout_ms",
            default,
            skip_serializing_if = "Option::is_none",
            with = "duration_ms::option"
        )]
        timeout: Option<Duration>,
    },

    ObserverAttached {
        observer: Observer,
    },

    ObserverDetached {
        observer: Observer,
    },

    /// The last claiming observer detached.
    ClaimsExhausted,

    LivenessDecided {
        policy: LivenessPolicy,
    },

    /// The run stopped resumable. Terminal.
    Parked {
        checkpoint: CheckpointRef,
    },

    /// The run completed its work. Terminal.
    Finished {
        /// Cumulative provider-billed tokens across every turn.
        #[serde(flatten)]
        usage: TokenUsage,
    },

    /// The run was ended before completing. Terminal.
    Cancelled {
        #[serde(flatten)]
        reason: RunCancelReason,
        /// The caller's own words.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    /// The run ended in an error. Terminal.
    Failed {
        error: String,
    },
}

impl RunLifecycleEvent {
    /// Whether the run emits nothing after this.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Parked { .. }
                | Self::Finished { .. }
                | Self::Cancelled { .. }
                | Self::Failed { .. }
        )
    }
}

/// Either half of what a run's stream carries.
#[expect(
    clippy::large_enum_variant,
    reason = "agent events are nearly every event on a run's stream; boxing \
              them would add an allocation to each to save memory only on \
              the rare lifecycle ones"
)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunEventPayload {
    Agent(AgentEvent),
    Lifecycle(RunLifecycleEvent),
}

/// One event in a run's ordered stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub seq: SequenceNumber,
    pub at: Timestamp,
    pub payload: RunEventPayload,
}

impl RunEvent {
    /// Whether the run emits nothing after this.
    pub fn is_terminal(&self) -> bool {
        match &self.payload {
            RunEventPayload::Lifecycle(lifecycle) => lifecycle.is_terminal(),
            _ => false,
        }
    }
}

/// Serializes a [`Duration`] as whole milliseconds, the unit every other
/// duration in this crate is expressed in on the wire.
mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }

    pub mod option {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        pub fn serialize<S: Serializer>(
            duration: &Option<Duration>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            duration
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .serialize(serializer)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Duration>, D::Error> {
            Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_millis))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEventPayload;
    use crate::{AgentContext, TokenCount};
    use serde_json::json;

    fn envelope(seq: u64, payload: RunEventPayload) -> RunEvent {
        RunEvent {
            run_id: RunId::new("run_1"),
            session_id: Some(SessionId::new("sess_1")),
            seq: SequenceNumber(seq),
            at: Timestamp::from_unix_millis(1_700_000_000_000),
            payload,
        }
    }

    fn roundtrip(event: &RunEvent) -> RunEvent {
        let json = serde_json::to_string(event).expect("run event should serialize");
        serde_json::from_str(&json)
            .unwrap_or_else(|err| panic!("run event should deserialize: {err}\n{json}"))
    }

    fn lifecycle(seq: u64, event: RunLifecycleEvent) -> RunEvent {
        envelope(seq, RunEventPayload::Lifecycle(event))
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            prompt_tokens: TokenCount::new(10),
            completion_tokens: TokenCount::new(5),
            total_tokens: TokenCount::new(15),
        }
    }

    fn collector() -> Observer {
        Observer {
            id: ObserverId::new("obs_1"),
            kind: ObserverKind::Collecting,
            presence: false,
        }
    }

    /// An observer is one value under its own key, so its `kind` can never
    /// collide with the envelope's `kind` tag beside it.
    #[test]
    fn an_observer_is_nested_under_its_own_key() {
        let json = serde_json::to_value(lifecycle(
            2,
            RunLifecycleEvent::ObserverAttached {
                observer: Observer {
                    id: ObserverId::new("obs_1"),
                    kind: ObserverKind::Claiming,
                    presence: true,
                },
            },
        ))
        .unwrap();

        assert_eq!(
            json["payload"],
            json!({
                "kind": "lifecycle",
                "type": "observer_attached",
                "observer": { "id": "obs_1", "kind": "claiming", "presence": true }
            })
        );
    }

    /// The correlation `AgentEvent` leaves off — run, session — sits on the
    /// envelope, beside the ordering a consumer needs, and the agent's own
    /// event is carried whole rather than reshaped.
    #[test]
    fn an_agent_event_travels_inside_the_envelope_untouched() {
        let event = envelope(
            3,
            RunEventPayload::Agent(AgentEvent::single_agent(AgentEventPayload::TextDelta {
                content: "hi".to_string(),
            })),
        );
        let json = serde_json::to_value(&event).expect("should serialize");

        assert_eq!(
            json,
            json!({
                "run_id": "run_1",
                "session_id": "sess_1",
                "seq": 3,
                "at": 1_700_000_000_000_u64,
                "payload": {
                    "kind": "agent",
                    "agent": { "agent_id": "main" },
                    "payload": { "type": "text_delta", "content": "hi" }
                }
            })
        );

        let RunEventPayload::Agent(inner) = roundtrip(&event).payload else {
            panic!("expected an agent event");
        };
        assert!(inner.agent.is_single_agent());
        assert!(matches!(inner.payload, AgentEventPayload::TextDelta { .. }));
    }

    /// A lifecycle event shares the envelope, told apart by `kind` and then
    /// by its own `type`, so a consumer switches on two tags and never on
    /// field presence.
    #[test]
    fn a_lifecycle_event_is_tagged_apart_from_agent_activity() {
        let json = serde_json::to_value(lifecycle(
            1,
            RunLifecycleEvent::Started {
                agent: "sre".to_string(),
                prompt: "why is checkout slow".to_string(),
                timeout: Some(Duration::from_secs(300)),
            },
        ))
        .expect("should serialize");

        assert_eq!(
            json["payload"],
            json!({
                "kind": "lifecycle",
                "type": "started",
                "agent": "sre",
                "prompt": "why is checkout slow",
                "timeout_ms": 300_000
            })
        );
    }

    #[test]
    fn every_lifecycle_variant_survives_a_roundtrip() {
        let variants = vec![
            RunLifecycleEvent::Started {
                agent: "sre".to_string(),
                prompt: "hi".to_string(),
                timeout: None,
            },
            RunLifecycleEvent::ObserverAttached {
                observer: Observer {
                    id: ObserverId::new("obs_1"),
                    kind: ObserverKind::Claiming,
                    presence: true,
                },
            },
            RunLifecycleEvent::ObserverDetached {
                observer: Observer {
                    id: ObserverId::new("obs_1"),
                    kind: ObserverKind::Collecting,
                    presence: false,
                },
            },
            RunLifecycleEvent::ClaimsExhausted,
            RunLifecycleEvent::LivenessDecided {
                policy: LivenessPolicy::Park,
            },
            RunLifecycleEvent::Parked {
                checkpoint: CheckpointRef::new("memory/sess_1/parked/run_1.json"),
            },
            RunLifecycleEvent::Finished { usage: usage() },
            RunLifecycleEvent::Cancelled {
                reason: RunCancelReason::Deadline {
                    after: Duration::from_millis(1500),
                },
                message: None,
            },
            RunLifecycleEvent::Cancelled {
                reason: RunCancelReason::External,
                message: Some("operator stopped it".to_string()),
            },
            RunLifecycleEvent::Cancelled {
                reason: RunCancelReason::ClientTool,
                message: None,
            },
            RunLifecycleEvent::Failed {
                error: "provider returned 500".to_string(),
            },
        ];

        for (i, variant) in variants.into_iter().enumerate() {
            let before = lifecycle(i as u64 + 1, variant);
            let before_json = serde_json::to_value(&before).unwrap();
            let after_json = serde_json::to_value(roundtrip(&before)).unwrap();
            assert_eq!(
                before_json, after_json,
                "variant {i} changed across a roundtrip"
            );
        }
    }

    /// A unit variant under an internal tag is just its tag, so the event with
    /// nothing to say still parses.
    #[test]
    fn claims_exhausted_carries_only_its_tag() {
        let json = serde_json::to_value(lifecycle(2, RunLifecycleEvent::ClaimsExhausted)).unwrap();
        assert_eq!(
            json["payload"],
            json!({ "kind": "lifecycle", "type": "claims_exhausted" })
        );

        let parsed: RunEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(
            parsed.payload,
            RunEventPayload::Lifecycle(RunLifecycleEvent::ClaimsExhausted)
        ));
    }

    /// The cancel reason flattens under the event the way a tool outcome
    /// flattens under `tool_complete`, and the caller's words ride beside it.
    #[test]
    fn a_cancel_reason_flattens_beside_the_callers_message() {
        let json = serde_json::to_value(lifecycle(
            9,
            RunLifecycleEvent::Cancelled {
                reason: RunCancelReason::Deadline {
                    after: Duration::from_secs(300),
                },
                message: Some("request timeout".to_string()),
            },
        ))
        .unwrap();

        assert_eq!(
            json["payload"],
            json!({
                "kind": "lifecycle",
                "type": "cancelled",
                "reason": "deadline",
                "after_ms": 300_000,
                "message": "request timeout"
            })
        );

        let external = serde_json::to_value(lifecycle(
            9,
            RunLifecycleEvent::Cancelled {
                reason: RunCancelReason::External,
                message: None,
            },
        ))
        .unwrap();
        assert_eq!(
            external["payload"],
            json!({ "kind": "lifecycle", "type": "cancelled", "reason": "external" })
        );
    }

    /// Durations go on the wire as whole milliseconds; anything finer is
    /// dropped, and nothing rounds up.
    #[test]
    fn a_duration_is_whole_milliseconds_on_the_wire() {
        let before = RunCancelReason::Deadline {
            after: Duration::from_micros(1_500_999),
        };
        let json = serde_json::to_value(before).unwrap();
        assert_eq!(json, json!({ "reason": "deadline", "after_ms": 1500 }));

        let after: RunCancelReason = serde_json::from_value(json).unwrap();
        assert_eq!(
            after,
            RunCancelReason::Deadline {
                after: Duration::from_millis(1500)
            }
        );
    }

    /// A run with no session omits the field, and a stream written that way
    /// parses back to `None` rather than to an empty id.
    #[test]
    fn a_run_without_a_session_omits_the_field() {
        let mut event = lifecycle(1, RunLifecycleEvent::ClaimsExhausted);
        event.session_id = None;

        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("session_id").is_none());
        assert_eq!(roundtrip(&event).session_id, None);
    }

    #[test]
    fn ids_serialize_as_bare_strings() {
        assert_eq!(
            serde_json::to_value(RunId::new("run_1")).unwrap(),
            json!("run_1")
        );
        assert_eq!(
            serde_json::to_value(SessionId::new("sess_1")).unwrap(),
            json!("sess_1")
        );
        assert_eq!(
            serde_json::to_value(ObserverId::new("obs_1")).unwrap(),
            json!("obs_1")
        );
        assert_eq!(
            serde_json::to_value(CheckpointRef::new("parked/run_1.json")).unwrap(),
            json!("parked/run_1.json")
        );
    }

    /// Dense numbering is what lets a consumer tell a missed event from a
    /// quiet run: the next number is always exactly one more.
    #[test]
    fn sequence_numbers_are_dense() {
        let first = SequenceNumber::FIRST;
        assert_eq!(first.get(), 1);

        let second = first.next();
        assert!(second.follows(first));
        assert!(!second.next().follows(first), "a skipped number is a gap");
        assert!(!first.follows(second), "order matters");
        assert!(!first.follows(first), "a repeat is not a successor");
    }

    #[test]
    fn a_sequence_number_serializes_as_a_bare_integer() {
        let json = serde_json::to_value(SequenceNumber::FIRST.next()).unwrap();
        assert_eq!(json, json!(2));
        assert_eq!(
            serde_json::from_value::<SequenceNumber>(json).unwrap(),
            SequenceNumber(2)
        );
    }

    #[test]
    fn a_timestamp_is_unix_milliseconds() {
        let at = Timestamp::from_unix_millis(1_700_000_000_000);
        assert_eq!(
            serde_json::to_value(at).unwrap(),
            json!(1_700_000_000_000_u64)
        );
        assert_eq!(at.unix_millis(), 1_700_000_000_000);

        let now = Timestamp::now();
        assert!(
            now > at,
            "now ({now}) should be after November 2023 ({at}) on any sane clock"
        );
    }

    /// A consumer that closes its stream on a terminal event must agree with
    /// the vocabulary about which events those are.
    #[test]
    fn terminal_events_end_the_run_and_the_rest_do_not() {
        let terminal = [
            RunLifecycleEvent::Parked {
                checkpoint: CheckpointRef::new("c"),
            },
            RunLifecycleEvent::Finished { usage: usage() },
            RunLifecycleEvent::Cancelled {
                reason: RunCancelReason::External,
                message: None,
            },
            RunLifecycleEvent::Failed {
                error: "boom".to_string(),
            },
        ];
        for event in terminal {
            assert!(event.is_terminal(), "{event:?} should be terminal");
            assert!(lifecycle(1, event).is_terminal());
        }

        let ongoing = [
            RunLifecycleEvent::Started {
                agent: "sre".to_string(),
                prompt: "hi".to_string(),
                timeout: None,
            },
            RunLifecycleEvent::ObserverAttached {
                observer: collector(),
            },
            RunLifecycleEvent::ObserverDetached {
                observer: collector(),
            },
            RunLifecycleEvent::ClaimsExhausted,
            RunLifecycleEvent::LivenessDecided {
                policy: LivenessPolicy::Continue,
            },
        ];
        for event in ongoing {
            assert!(!event.is_terminal(), "{event:?} should not be terminal");
        }

        let activity = envelope(
            1,
            RunEventPayload::Agent(AgentEvent::new(
                AgentContext::single_agent(),
                AgentEventPayload::TextDelta {
                    content: "done".to_string(),
                },
            )),
        );
        assert!(
            !activity.is_terminal(),
            "an agent saying it is done is not the run ending"
        );
    }

    /// The agent's `RunParked` and the lifecycle `Parked` both appear in a
    /// parked run's stream, and neither is mistaken for the other.
    #[test]
    fn the_agents_run_parked_and_the_lifecycle_parked_are_distinct() {
        let from_agent = envelope(
            7,
            RunEventPayload::Agent(AgentEvent::new(
                AgentContext::coordinator(),
                AgentEventPayload::RunParked {
                    run_id: "run_1".to_string(),
                    decision_ids: vec!["d1".to_string()],
                    expires_at: "2026-09-24T14:00:00Z".to_string(),
                    iteration: 2,
                },
            )),
        );
        let from_owner = lifecycle(
            8,
            RunLifecycleEvent::Parked {
                checkpoint: CheckpointRef::new("memory/sess_1/parked/run_1.json"),
            },
        );

        let agent_json = serde_json::to_value(&from_agent).unwrap();
        let owner_json = serde_json::to_value(&from_owner).unwrap();
        assert_eq!(agent_json["payload"]["kind"], "agent");
        assert_eq!(agent_json["payload"]["payload"]["type"], "run_parked");
        assert_eq!(owner_json["payload"]["kind"], "lifecycle");
        assert_eq!(owner_json["payload"]["type"], "parked");

        assert!(!from_agent.is_terminal());
        assert!(from_owner.is_terminal());
    }
}
