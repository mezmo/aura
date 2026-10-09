//! A session's event stream: what its agents did and what happened to its runs.
//!
//! [`AgentEvent`] says what an agent did. It says nothing about the run it
//! belongs to — that it started, who is watching, that it finished or was
//! cancelled and why. A [`SessionEvent`] is the envelope that carries both, so
//! a projection reads one ordered sequence and never correlates two.
//!
//! # Agent, session, run
//!
//! An agent is a config entry. A session is that agent's identity over time,
//! and the key its stream is ordered under. A run is one unit of work in a
//! session: a prompt or headless start, through every inference and tool turn,
//! to one terminal state. One stream per session means an agent's whole
//! history loads as one stream rather than as a composition of runs, and a run
//! boundary is a position in it.
//!
//! # Correlation lives on the envelope
//!
//! The session and run ids that [`AgentEvent`] deliberately omits are here,
//! applied once by whatever owns the session rather than by every observer.
//! Every event names its session. An event names a run when it belongs to
//! one; an observer attaching to an idle session belongs to none.
//!
//! A run has one id, a [`RunId`] minted once when the run starts.
//!
//! # Sequence numbers are dense
//!
//! Every event of a session carries a [`SequenceNumber`], starting at
//! [`SequenceNumber::FIRST`] and increasing by exactly one per event across
//! all of the session's runs. A consumer that sees a gap knows it missed
//! something rather than silently reading a partial stream. The number is
//! minted in one place — by whatever appends to the session's stream — never
//! by a relay or projection.
//!
//! Order by `seq`, never by `at`. The [`Timestamp`] is wall-clock time, for
//! display. It is Unix milliseconds, a number, where other instants on the wire,
//! such as an approval's `expires_at`, are RFC 3339 strings: it is stamped on
//! every event, and a consumer only displays it.
//!
//! # Tags and formats
//!
//! The payload is adjacently tagged: `kind` names the half, and the event sits
//! under `event`, so no field of any event can collide with the envelope's tag.
//! A lifecycle event is internally tagged by `type`, as
//! [`AgentEventPayload`](crate::agent::AgentEventPayload) is, so a lifecycle
//! field named `type` — or `reason`, which [`RunCancelReason`] flattens into
//! [`LifecycleEvent::Cancelled`] — would collide; the roundtrip test over
//! every variant is what catches one.
//!
//! A reader older than its producer meets variants it does not know. Every
//! tagged enum here reads any other tag as its `Unknown` variant, so the event
//! still parses and its `seq` still counts. [`SessionEventPayload`] is the
//! exception: serde cannot fall back on an adjacent tag that carries content,
//! and its two kinds are not expected to grow. An `Unknown` cannot be written
//! back, so a relay forwards the bytes it received, never a re-encoded event.
//!
//! Internal tagging and `#[serde(flatten)]` need a self-describing format.
//! These types round-trip through JSON or MessagePack, not `bincode` or
//! `postcard`, and a store that persists them inherits that.
//!
//! # Lifecycle beside activity
//!
//! [`AgentEventPayload::RunParked`](crate::agent::AgentEventPayload::RunParked)
//! is an agent event: the orchestrator emits it from inside the run with its
//! iteration state. [`LifecycleEvent::Parked`] is the run owner's
//! acknowledgement and carries the checkpoint reference. Both appear in the
//! stream; a projection decides which it surfaces.
//!
//! [`LifecycleEvent::Started`] carries the prompt, not the history. History
//! is the run's input, too large to replay to every late observer.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::AgentEvent;
use crate::{nonempty_string_newtype, RunId, SequenceNumber, SessionId, Timestamp, TokenUsage};

nonempty_string_newtype! {
    /// Identifier for one observer of a session, unique among its observers.
    ObserverId
}

nonempty_string_newtype! {
    /// Locates a parked run's checkpoint, as the park machinery names it.
    CheckpointRef
}

/// What an observer's subscription does to the live run's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ObserverKind {
    /// Reads the stream without a claim on any run.
    Collecting,
    /// Holds the live run's right to continue.
    Claiming,
    /// Any other value on the wire, read by a version of this crate that does
    /// not know it. It cannot be written back.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// One observer of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observer {
    pub id: ObserverId,
    pub kind: ObserverKind,
    /// Whether a human is at this observer.
    pub presence: bool,
}

/// Why an observer stopped observing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DetachCause {
    /// The observer let go.
    Released,
    /// A lease held across a process boundary lapsed unrenewed.
    Expired,
    /// Another claimant took over.
    Displaced,
    /// Any other value on the wire, read by a version of this crate that does
    /// not know it. It cannot be written back.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// What a run does once nothing claims it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LivenessPolicy {
    /// End the run.
    Cancel,
    /// Run to completion unclaimed.
    Continue,
    /// Checkpoint and end the run resumable.
    Park,
    /// Any other value on the wire, read by a version of this crate that does
    /// not know it. It cannot be written back.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// How a run reacts to going unclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Liveness {
    pub policy: LivenessPolicy,
    /// How long the run goes unclaimed before `policy` acts.
    #[serde(rename = "grace_ms", with = "crate::duration_ms")]
    pub grace: Duration,
}

impl Default for Liveness {
    /// Cancel at once: a run nobody claims spends provider turns for no one.
    fn default() -> Self {
        Self {
            policy: LivenessPolicy::Cancel,
            grace: Duration::ZERO,
        }
    }
}

/// Why a run was ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunCancelReason {
    /// The run outlived the bound it was started with.
    Deadline {
        #[serde(rename = "after_ms", with = "crate::duration_ms")]
        after: Duration,
    },
    /// Something outside the run cancelled it.
    External,
    /// The model called a tool the caller executes.
    ClientTool,
    /// Nothing claimed the run, and its liveness policy ended it.
    Unclaimed,
    /// The process stopped serving runs.
    Shutdown,
    /// Any other value on the wire, read by a version of this crate that does
    /// not know it. It cannot be written back.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// What happened to a session's run, or to the session itself, as distinct
/// from what its agent did.
///
/// `#[non_exhaustive]` because the vocabulary grows as the session's owner
/// learns to say more.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LifecycleEvent {
    Started {
        /// The configured agent running, matching
        /// [`AgentInfo::id`](crate::AgentInfo::id).
        agent: String,
        prompt: String,
        #[serde(
            rename = "timeout_ms",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::duration_ms::option"
        )]
        timeout: Option<Duration>,
        #[serde(default, deserialize_with = "null_as_default")]
        liveness: Liveness,
        /// The run this one continues, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continues: Option<RunId>,
    },

    ObserverAttached {
        observer: Observer,
    },

    ObserverDetached {
        observer: Observer,
        because: DetachCause,
    },

    /// The last claiming observer detached.
    ClaimsExhausted,

    LivenessDecided {
        policy: LivenessPolicy,
    },

    /// The run stopped resumable.
    Parked {
        checkpoint: CheckpointRef,
        /// As [`Finished`](Self::Finished)'s.
        #[serde(flatten)]
        usage: TokenUsage,
    },

    /// The run completed its work.
    Finished {
        /// Cumulative provider-billed tokens across every turn.
        #[serde(flatten)]
        usage: TokenUsage,
    },

    /// The run was ended before completing.
    Cancelled {
        #[serde(flatten)]
        reason: RunCancelReason,
        /// The caller's own words.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// As [`Finished`](Self::Finished)'s.
        #[serde(flatten)]
        usage: TokenUsage,
    },

    /// The run ended in an error.
    Failed {
        error: String,
        /// As [`Finished`](Self::Finished)'s.
        #[serde(flatten)]
        usage: TokenUsage,
    },

    /// Any other value on the wire, read by a version of this crate that does
    /// not know it. It cannot be written back.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// Reads `null` as the field's default, as a missing field already is.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

impl LifecycleEvent {
    /// Whether this ends its run: a park, a finish, a cancel, or a failure.
    /// The session's stream goes on past it.
    pub fn ends_run(&self) -> bool {
        matches!(
            self,
            Self::Parked { .. }
                | Self::Finished { .. }
                | Self::Cancelled { .. }
                | Self::Failed { .. }
        )
    }
}

/// Either half of what a session's stream carries.
#[expect(
    clippy::large_enum_variant,
    reason = "agent events are nearly every event on a session's stream; \
              boxing them would add an allocation to each to save memory \
              only on the rare lifecycle ones"
)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionEventPayload {
    Agent(AgentEvent),
    Lifecycle(LifecycleEvent),
}

/// One event in a session's ordered stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEvent {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub seq: SequenceNumber,
    pub at: Timestamp,
    pub payload: SessionEventPayload,
}

impl SessionEvent {
    /// See [`LifecycleEvent::ends_run`].
    pub fn ends_run(&self) -> bool {
        match &self.payload {
            SessionEventPayload::Lifecycle(lifecycle) => lifecycle.ends_run(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEventPayload;
    use crate::{AgentContext, TokenCount};
    use serde_json::json;

    const RUN: &str = "0191e8c0-1111-7000-8000-000000000001";

    fn run_id() -> RunId {
        RUN.parse().expect("a well-formed UUID")
    }

    fn envelope(seq: u64, run: Option<RunId>, payload: SessionEventPayload) -> SessionEvent {
        SessionEvent {
            session_id: SessionId::new("sess_1"),
            run_id: run,
            seq: SequenceNumber::try_from(seq).expect("a sequence starts at 1"),
            at: Timestamp::from_unix_millis(1_700_000_000_000),
            payload,
        }
    }

    fn lifecycle(seq: u64, event: LifecycleEvent) -> SessionEvent {
        envelope(seq, Some(run_id()), SessionEventPayload::Lifecycle(event))
    }

    fn roundtrip(event: &SessionEvent) -> SessionEvent {
        let json = serde_json::to_string(event).expect("session event should serialize");
        serde_json::from_str(&json)
            .unwrap_or_else(|err| panic!("session event should deserialize: {err}\n{json}"))
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            prompt_tokens: TokenCount::new(10),
            completion_tokens: TokenCount::new(5),
            total_tokens: TokenCount::new(15),
        }
    }

    fn observer(kind: ObserverKind, presence: bool) -> Observer {
        Observer {
            id: ObserverId::new("obs_1").unwrap(),
            kind,
            presence,
        }
    }

    /// The correlation `AgentEvent` leaves off — session, run — sits on the
    /// envelope, beside the ordering a consumer needs, and the agent's own
    /// event is carried whole under `event` rather than reshaped.
    #[test]
    fn an_agent_event_travels_inside_the_envelope_untouched() {
        let event = envelope(
            3,
            Some(run_id()),
            SessionEventPayload::Agent(AgentEvent::single_agent(AgentEventPayload::TextDelta {
                content: "hi".to_string(),
            })),
        );
        let json = serde_json::to_value(&event).expect("should serialize");

        assert_eq!(
            json,
            json!({
                "session_id": "sess_1",
                "run_id": RUN,
                "seq": 3,
                "at": 1_700_000_000_000_u64,
                "payload": {
                    "kind": "agent",
                    "event": {
                        "agent": { "agent_id": "main" },
                        "payload": { "type": "text_delta", "content": "hi" }
                    }
                }
            })
        );

        let SessionEventPayload::Agent(inner) = roundtrip(&event).payload else {
            panic!("expected an agent event");
        };
        assert!(inner.agent.is_single_agent());
        assert!(matches!(inner.payload, AgentEventPayload::TextDelta { .. }));
    }

    /// A lifecycle event shares the envelope, told apart by `kind` and then
    /// by its own `type`, so a consumer switches on two tags and never on
    /// field presence.
    #[test]
    fn a_lifecycle_event_nests_under_its_kind() {
        let json = serde_json::to_value(lifecycle(
            1,
            LifecycleEvent::Started {
                agent: "sre".to_string(),
                prompt: "why is checkout slow".to_string(),
                timeout: Some(Duration::from_secs(300)),
                liveness: Liveness {
                    policy: LivenessPolicy::Continue,
                    grace: Duration::from_secs(30),
                },
                continues: None,
            },
        ))
        .expect("should serialize");

        assert_eq!(
            json["payload"],
            json!({
                "kind": "lifecycle",
                "event": {
                    "type": "started",
                    "agent": "sre",
                    "prompt": "why is checkout slow",
                    "timeout_ms": 300_000,
                    "liveness": { "policy": "continue", "grace_ms": 30_000 }
                }
            })
        );
    }

    /// An observer attaching to a session with no live run belongs to no run,
    /// and the envelope says so by omission rather than with an empty id.
    #[test]
    fn an_event_outside_any_run_omits_the_run_id() {
        let event = envelope(
            1,
            None,
            SessionEventPayload::Lifecycle(LifecycleEvent::ObserverAttached {
                observer: observer(ObserverKind::Collecting, true),
            }),
        );

        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("run_id").is_none());
        assert_eq!(json["session_id"], "sess_1");
        assert_eq!(roundtrip(&event).run_id, None);
    }

    /// Every lifecycle variant, every cancel reason and every detach cause
    /// survives a roundtrip. A field named after a tag — `type` on a lifecycle
    /// variant, `reason` beside a flattened cancel reason — serializes as a
    /// duplicate key and fails here.
    #[test]
    fn every_lifecycle_variant_survives_a_roundtrip() {
        let variants = vec![
            LifecycleEvent::Started {
                agent: "sre".to_string(),
                prompt: "hi".to_string(),
                timeout: None,
                liveness: Liveness::default(),
                continues: None,
            },
            LifecycleEvent::ObserverAttached {
                observer: observer(ObserverKind::Claiming, true),
            },
            LifecycleEvent::ObserverDetached {
                observer: observer(ObserverKind::Collecting, false),
                because: DetachCause::Released,
            },
            LifecycleEvent::ObserverDetached {
                observer: observer(ObserverKind::Claiming, false),
                because: DetachCause::Expired,
            },
            LifecycleEvent::ObserverDetached {
                observer: observer(ObserverKind::Claiming, true),
                because: DetachCause::Displaced,
            },
            LifecycleEvent::ClaimsExhausted,
            LifecycleEvent::LivenessDecided {
                policy: LivenessPolicy::Park,
            },
            LifecycleEvent::Parked {
                checkpoint: CheckpointRef::new("memory/sess_1/parked/run_1.json").unwrap(),
                usage: usage(),
            },
            LifecycleEvent::Finished { usage: usage() },
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::Deadline {
                    after: Duration::from_millis(1500),
                },
                message: None,
                usage: usage(),
            },
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::External,
                message: Some("operator stopped it".to_string()),
                usage: usage(),
            },
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::ClientTool,
                message: None,
                usage: usage(),
            },
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::Unclaimed,
                message: None,
                usage: usage(),
            },
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::Shutdown,
                message: None,
                usage: usage(),
            },
            LifecycleEvent::Failed {
                error: "provider returned 500".to_string(),
                usage: usage(),
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

    /// The observer's `kind` sits inside the observer, inside the event, so
    /// it never meets the envelope's `kind`; the detach says why it left.
    #[test]
    fn a_detach_names_its_observer_and_why_it_left() {
        let json = serde_json::to_value(lifecycle(
            2,
            LifecycleEvent::ObserverDetached {
                observer: observer(ObserverKind::Claiming, true),
                because: DetachCause::Displaced,
            },
        ))
        .unwrap();

        assert_eq!(
            json["payload"],
            json!({
                "kind": "lifecycle",
                "event": {
                    "type": "observer_detached",
                    "observer": { "id": "obs_1", "kind": "claiming", "presence": true },
                    "because": "displaced"
                }
            })
        );
    }

    /// A unit variant under an internal tag is just its tag, so the event with
    /// nothing to say still parses.
    #[test]
    fn claims_exhausted_carries_only_its_tag() {
        let json = serde_json::to_value(lifecycle(2, LifecycleEvent::ClaimsExhausted)).unwrap();
        assert_eq!(
            json["payload"],
            json!({ "kind": "lifecycle", "event": { "type": "claims_exhausted" } })
        );

        let parsed: SessionEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(
            parsed.payload,
            SessionEventPayload::Lifecycle(LifecycleEvent::ClaimsExhausted)
        ));
    }

    /// The cancel reason flattens under the event the way a tool outcome
    /// flattens under `tool_complete`, and the caller's words ride beside it.
    #[test]
    fn a_cancel_reason_flattens_beside_the_callers_message() {
        let json = serde_json::to_value(lifecycle(
            9,
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::Deadline {
                    after: Duration::from_secs(300),
                },
                message: Some("request timeout".to_string()),
                usage: usage(),
            },
        ))
        .unwrap();

        assert_eq!(
            json["payload"]["event"],
            json!({
                "type": "cancelled",
                "reason": "deadline",
                "after_ms": 300_000,
                "message": "request timeout",
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            })
        );

        for (reason, tag) in [
            (RunCancelReason::External, "external"),
            (RunCancelReason::Unclaimed, "unclaimed"),
            (RunCancelReason::Shutdown, "shutdown"),
        ] {
            let json = serde_json::to_value(lifecycle(
                9,
                LifecycleEvent::Cancelled {
                    reason,
                    message: None,
                    usage: usage(),
                },
            ))
            .unwrap();
            assert_eq!(
                json["payload"]["event"],
                json!({
                    "type": "cancelled",
                    "reason": tag,
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                })
            );
        }
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

    /// A start that omits its liveness parses, and reads as the default:
    /// cancel at once.
    #[test]
    fn a_start_without_liveness_reads_as_cancel_at_once() {
        let parsed: SessionEvent = serde_json::from_value(json!({
            "session_id": "sess_1",
            "run_id": RUN,
            "seq": 1,
            "at": 0,
            "payload": {
                "kind": "lifecycle",
                "event": { "type": "started", "agent": "sre", "prompt": "hi" }
            }
        }))
        .unwrap();

        let SessionEventPayload::Lifecycle(LifecycleEvent::Started { liveness, .. }) =
            parsed.payload
        else {
            panic!("expected a start");
        };
        assert_eq!(liveness, Liveness::default());
        assert_eq!(liveness.policy, LivenessPolicy::Cancel);
        assert_eq!(liveness.grace, Duration::ZERO);
    }

    /// A run that continues another names it; one that does not carries no
    /// `continues` on the wire, so an older payload reads as a fresh run.
    #[test]
    fn a_continuing_start_names_the_run_it_follows() {
        let parked = run_id();
        let start = LifecycleEvent::Started {
            agent: "sre".to_string(),
            prompt: "hi".to_string(),
            timeout: None,
            liveness: Liveness::default(),
            continues: Some(parked),
        };
        let json = serde_json::to_value(&start).unwrap();
        assert_eq!(json["continues"], RUN);

        let LifecycleEvent::Started { continues, .. } = serde_json::from_value(json).unwrap()
        else {
            panic!("expected a start");
        };
        assert_eq!(continues, Some(parked));

        let fresh = serde_json::to_value(LifecycleEvent::Started {
            agent: "sre".to_string(),
            prompt: "hi".to_string(),
            timeout: None,
            liveness: Liveness::default(),
            continues: None,
        })
        .unwrap();
        assert!(fresh.get("continues").is_none());
    }

    /// Every terminal event carries the run's cumulative usage at the top
    /// level, as `Finished` does, so cost reads the same way off any of them.
    #[test]
    fn every_terminal_event_carries_usage_the_same_way() {
        let terminal = [
            LifecycleEvent::Parked {
                checkpoint: CheckpointRef::new("c").unwrap(),
                usage: usage(),
            },
            LifecycleEvent::Finished { usage: usage() },
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::External,
                message: None,
                usage: usage(),
            },
            LifecycleEvent::Failed {
                error: "boom".to_string(),
                usage: usage(),
            },
        ];
        for event in terminal {
            assert!(event.ends_run());
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["prompt_tokens"], 10, "{json}");
            assert_eq!(json["completion_tokens"], 5, "{json}");
            assert_eq!(json["total_tokens"], 15, "{json}");
        }
    }

    /// A producer that writes an absent liveness as `null` gets the same
    /// default as one that omits it.
    #[test]
    fn a_start_with_a_null_liveness_reads_as_cancel_at_once() {
        let parsed: LifecycleEvent = serde_json::from_value(json!({
            "type": "started",
            "agent": "sre",
            "prompt": "hi",
            "timeout_ms": null,
            "liveness": null
        }))
        .unwrap();

        let LifecycleEvent::Started {
            liveness, timeout, ..
        } = parsed
        else {
            panic!("expected a start");
        };
        assert_eq!(liveness, Liveness::default());
        assert_eq!(timeout, None);
    }

    #[test]
    fn ids_serialize_as_bare_strings() {
        assert_eq!(serde_json::to_value(run_id()).unwrap(), json!(RUN));
        assert_eq!(
            serde_json::to_value(SessionId::new("sess_1")).unwrap(),
            json!("sess_1")
        );
        assert_eq!(
            serde_json::to_value(ObserverId::new("obs_1").unwrap()).unwrap(),
            json!("obs_1")
        );
        assert_eq!(
            serde_json::to_value(CheckpointRef::new("parked/run_1.json").unwrap()).unwrap(),
            json!("parked/run_1.json")
        );
    }

    /// A run id is a UUID on the way in as well as out, so an id minted as
    /// anything else is refused at the boundary rather than carried along.
    #[test]
    fn a_run_id_that_is_not_a_uuid_is_refused() {
        assert!(serde_json::from_value::<RunId>(json!("req_1")).is_err());
        assert!("req_1".parse::<RunId>().is_err());
        assert_eq!(
            serde_json::from_value::<RunId>(json!(RUN)).unwrap(),
            run_id()
        );
    }

    /// The nil UUID is the UUID that names nothing, so it is refused on every
    /// path a run id can be built by.
    #[test]
    fn the_nil_uuid_is_not_a_run_id() {
        const NIL: &str = "00000000-0000-0000-0000-000000000000";

        assert_eq!(NIL.parse::<RunId>(), Err(crate::InvalidRunId::Nil));
        assert_eq!(
            RunId::try_from(uuid::Uuid::nil()),
            Err(crate::InvalidRunId::Nil)
        );
        let err = serde_json::from_value::<RunId>(json!(NIL)).unwrap_err();
        assert!(err.to_string().contains("nil UUID"), "got: {err}");
    }

    /// An observer or checkpoint named by the empty string names nothing, so
    /// neither the constructor nor deserialization will build one.
    #[test]
    fn an_empty_observer_or_checkpoint_id_is_refused() {
        let err = ObserverId::new("").unwrap_err();
        assert_eq!(err.to_string(), "ObserverId cannot be empty");
        assert!(CheckpointRef::new(String::new()).is_err());
        assert!(ObserverId::try_from("").is_err());

        assert!(serde_json::from_value::<ObserverId>(json!("")).is_err());
        assert!(serde_json::from_value::<CheckpointRef>(json!("")).is_err());
        assert_eq!(
            serde_json::from_value::<ObserverId>(json!("obs_1")).unwrap(),
            "obs_1"
        );
    }

    /// The refusal reaches the envelope: an attach naming an empty observer
    /// does not parse into an event.
    #[test]
    fn an_event_naming_an_empty_observer_does_not_parse() {
        let parsed = serde_json::from_value::<SessionEvent>(json!({
            "session_id": "sess_1",
            "seq": 1,
            "at": 0,
            "payload": {
                "kind": "lifecycle",
                "event": {
                    "type": "observer_attached",
                    "observer": { "id": "", "kind": "collecting", "presence": false }
                }
            }
        }));
        assert!(parsed.is_err());
    }

    /// Version 7 puts the mint time in the leading bits, which is what lets
    /// ids order by when their runs started.
    #[test]
    fn a_minted_run_id_is_version_7() {
        assert_eq!(RunId::mint().as_uuid().get_version_num(), 7);
        assert_ne!(RunId::mint(), RunId::mint());
    }

    /// A consumer that closes a run's view on its last event must agree with
    /// the vocabulary about which events those are.
    #[test]
    fn only_a_terminal_lifecycle_event_ends_a_run() {
        let ending = [
            LifecycleEvent::Parked {
                checkpoint: CheckpointRef::new("c").unwrap(),
                usage: usage(),
            },
            LifecycleEvent::Finished { usage: usage() },
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::External,
                message: None,
                usage: usage(),
            },
            LifecycleEvent::Failed {
                error: "boom".to_string(),
                usage: usage(),
            },
        ];
        for event in ending {
            assert!(event.ends_run(), "{event:?} should end its run");
            assert!(lifecycle(1, event).ends_run());
        }

        let ongoing = [
            LifecycleEvent::Started {
                agent: "sre".to_string(),
                prompt: "hi".to_string(),
                timeout: None,
                liveness: Liveness::default(),
                continues: None,
            },
            LifecycleEvent::ObserverAttached {
                observer: observer(ObserverKind::Collecting, false),
            },
            LifecycleEvent::ObserverDetached {
                observer: observer(ObserverKind::Collecting, false),
                because: DetachCause::Released,
            },
            LifecycleEvent::ClaimsExhausted,
            LifecycleEvent::LivenessDecided {
                policy: LivenessPolicy::Continue,
            },
        ];
        for event in ongoing {
            assert!(!event.ends_run(), "{event:?} should not end its run");
        }

        let activity = envelope(
            1,
            Some(run_id()),
            SessionEventPayload::Agent(AgentEvent::new(
                AgentContext::single_agent(),
                AgentEventPayload::TextDelta {
                    content: "done".to_string(),
                },
            )),
        );
        assert!(
            !activity.ends_run(),
            "an agent saying it is done is not the run ending"
        );
    }

    /// The agent's `RunParked` and the lifecycle `Parked` both appear in a
    /// parked run's stream, and neither is mistaken for the other.
    #[test]
    fn the_agents_run_parked_and_the_lifecycle_parked_are_distinct() {
        let from_agent = envelope(
            7,
            Some(run_id()),
            SessionEventPayload::Agent(AgentEvent::new(
                AgentContext::coordinator(),
                AgentEventPayload::RunParked {
                    run_id: RUN.to_string(),
                    decision_ids: vec!["d1".to_string()],
                    expires_at: "2026-09-24T14:00:00Z".to_string(),
                    iteration: 2,
                },
            )),
        );
        let from_owner = lifecycle(
            8,
            LifecycleEvent::Parked {
                checkpoint: CheckpointRef::new("memory/sess_1/parked/run_1.json").unwrap(),
                usage: usage(),
            },
        );

        let agent_json = serde_json::to_value(&from_agent).unwrap();
        let owner_json = serde_json::to_value(&from_owner).unwrap();
        assert_eq!(agent_json["payload"]["kind"], "agent");
        assert_eq!(
            agent_json["payload"]["event"]["payload"]["type"],
            "run_parked"
        );
        assert_eq!(owner_json["payload"]["kind"], "lifecycle");
        assert_eq!(owner_json["payload"]["event"]["type"], "parked");

        assert!(!from_agent.ends_run());
        assert!(from_owner.ends_run());
    }

    /// A reader older than its producer keeps the stream: a variant it does
    /// not know reads as `Unknown`, and the event's `seq` still counts.
    #[test]
    fn an_unknown_variant_reads_as_unknown_and_keeps_the_envelope() {
        let event: SessionEvent = serde_json::from_value(json!({
            "session_id": "sess_1",
            "run_id": RUN,
            "seq": 7,
            "at": 0,
            "payload": {
                "kind": "lifecycle",
                "event": { "type": "suspended", "until_ms": 5 }
            }
        }))
        .unwrap();
        assert_eq!(event.seq.get(), 7);
        assert!(matches!(
            event.payload,
            SessionEventPayload::Lifecycle(LifecycleEvent::Unknown)
        ));
        assert!(
            !event.ends_run(),
            "an unknown event is not known to end its run"
        );

        let cancelled: LifecycleEvent = serde_json::from_value(json!({
            "type": "cancelled",
            "reason": "preempted",
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }))
        .unwrap();
        assert!(matches!(
            cancelled,
            LifecycleEvent::Cancelled {
                reason: RunCancelReason::Unknown,
                message: None,
                ..
            }
        ));

        let started: LifecycleEvent = serde_json::from_value(json!({
            "type": "started",
            "agent": "sre",
            "prompt": "hi",
            "liveness": { "policy": "hibernate", "grace_ms": 1 }
        }))
        .unwrap();
        assert!(matches!(
            started,
            LifecycleEvent::Started {
                liveness: Liveness {
                    policy: LivenessPolicy::Unknown,
                    ..
                },
                ..
            }
        ));

        let detached: LifecycleEvent = serde_json::from_value(json!({
            "type": "observer_detached",
            "observer": { "id": "obs_1", "kind": "mirroring", "presence": false },
            "because": "preempted"
        }))
        .unwrap();
        assert!(matches!(
            detached,
            LifecycleEvent::ObserverDetached {
                observer: Observer {
                    kind: ObserverKind::Unknown,
                    ..
                },
                because: DetachCause::Unknown
            }
        ));
    }

    /// An `Unknown` is read, never written: a relay forwards the bytes it
    /// received rather than re-encoding an event it could not read.
    #[test]
    fn an_unknown_variant_cannot_be_serialized() {
        assert!(serde_json::to_value(LifecycleEvent::Unknown).is_err());
        assert!(serde_json::to_value(RunCancelReason::Unknown).is_err());
        assert!(serde_json::to_value(LivenessPolicy::Unknown).is_err());
        assert!(serde_json::to_value(ObserverKind::Unknown).is_err());
        assert!(serde_json::to_value(DetachCause::Unknown).is_err());

        let event = envelope(
            7,
            Some(run_id()),
            SessionEventPayload::Lifecycle(LifecycleEvent::Unknown),
        );
        assert!(serde_json::to_value(&event).is_err());
    }

    /// The payload's `kind` is the one tag with no fallback.
    #[test]
    fn an_unknown_payload_kind_is_refused() {
        let parsed = serde_json::from_value::<SessionEvent>(json!({
            "session_id": "sess_1",
            "seq": 1,
            "at": 0,
            "payload": { "kind": "telemetry", "event": {} }
        }));
        assert!(parsed.is_err());
    }
}
