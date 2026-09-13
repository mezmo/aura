//! The provider-valid context builder for the reconstruction direction
//! (P45, R5): one awaiting node's parked conversation rebuilt so the
//! continuation streams a context in which every decided call's tool
//! result is preceded by the assistant tool call of the same id — the
//! pairing providers require; an orphaned tool result is a protocol
//! violation everywhere the chain speaks.
//!
//! The bundle is built in two phases, because the outcome wires exist
//! only after the segment invokes the decided calls:
//!
//! 1. **Preflight (fallible).** [`ValidatedCalls::try_new`] parses one
//!    awaiting node's pending calls — ordered, non-empty, every call id
//!    non-empty and unique within the node. The substitution prelude
//!    runs it for every awaiting node of the segment BEFORE any
//!    invocation, so one bad call id refuses the whole segment before a
//!    tool runs (the gate's park arm can stamp an empty call id when
//!    the stream hook observed no tool-call id).
//! 2. **Resolution (positional).** [`ValidatedCalls::resolve`] pairs
//!    each call with the outcome wire its invocation produced — the
//!    i-th outcome with the i-th call in document order, so calls and
//!    outcomes cannot pair in the wrong order.
//!
//! [`rebuild_context`] then rebuilds the node's `(history,
//! current_prompt)`: for every bundle call in document order, the
//! assistant tool call is present in the history — reused where the
//! snapshot captured it, synthesized from the call's own record where
//! the park missed it — and the prompt carries exactly one tool result
//! keyed to the call's own id, carrying its outcome wire verbatim:
//! replacing the sentinel slot where one exists, appended where none
//! does. The captured history is otherwise byte-preserved, and tool
//! results the prompt carries for ids outside the bundle (a prior
//! resume's outcomes riding a re-parked snapshot) are preserved
//! verbatim.
//!
//! This stage-2 unit lays the type surface only: the three behavior
//! bodies are `todo!()` (filled by P45 stage 2b), the module is unwired
//! (P45 stage 3 points the substitution prelude at it; the plan's stage
//! 7 reaps `continuation::replace_tool_result` last), and the design
//! record is `REBUILD-DESIGN.md` beside this module.

#![allow(dead_code)] // unwired: P45 stage 3 wires the builder into the
// substitution prelude; sweep this allow together with the re-export's
// unused-imports marker in park/mod.rs when the wiring lands

use std::fmt;

use rig::completion::Message;
use serde_json::Value;

use super::resume::Diagnostic;
use crate::hitl::DecisionId;
use crate::orchestration::{ParkSnapshot, PendingCall};

/// A pending call's non-empty tool-call id: the key every rebuilt tool
/// result rides under and the id the reconstructed assistant tool call
/// carries. Constructed only inside this module, after the non-empty
/// check — an empty id (the shape the gate's park arm records when the
/// stream hook observed no tool-call id) has no `CallId`, so it cannot
/// reach any reconstruction keying.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CallId(String);

impl AsRef<str> for CallId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The outcome text one decided call's tool result carries, in the
/// chain's own wire rendering as the loop delivers it to the model —
/// rig JSON-serializes tool outputs, so an ordinary result arrives
/// JSON-quoted; an execution error renders raw; a denial short-circuits
/// with the live denial text. Total: every rendered text is a valid wire
/// form. The builder carries it verbatim and never re-renders, so no
/// outcome is double-encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutcomeWire(String);

impl OutcomeWire {
    /// Take the outcome in its wire form.
    #[must_use]
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

impl AsRef<str> for OutcomeWire {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// One pending call parsed for reconstruction: the decision it resolves
/// against, the gated tool's recorded name, the transformed arguments
/// the gate saw, and the call's validated [`CallId`]. Built only by
/// [`ValidatedCalls::try_new`]; everything but the id is the
/// checkpoint's record carried verbatim (the consult's mismatch row has
/// already compared tool and arguments against the stored approval).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValidatedCall {
    decision_id: DecisionId,
    tool_name: String,
    arguments: Value,
    call_id: CallId,
}

impl ValidatedCall {
    /// The call's validated tool-call id — the reconstruction's key.
    #[must_use]
    pub(crate) fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The gated tool's recorded name.
    #[must_use]
    pub(crate) fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// The transformed arguments the gate recorded.
    #[must_use]
    pub(crate) fn arguments(&self) -> &Value {
        &self.arguments
    }

    /// The decision the call's outcome resolves against.
    #[must_use]
    pub(crate) fn decision_id(&self) -> DecisionId {
        self.decision_id
    }
}

/// One awaiting node's pending calls, parsed for reconstruction:
/// ordered (the checkpoint's document order), non-empty, every call id
/// non-empty, and no id shared by two calls. The segment-wide preflight
/// constructs one per awaiting node before any invocation, so a single
/// empty call id refuses the whole segment before a tool runs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValidatedCalls {
    members: Vec<ValidatedCall>,
}

impl ValidatedCalls {
    /// Parse the node's pending calls. Refusals, in parse order: an
    /// empty list (a node with no pending calls faults earlier, in
    /// seeding — this constructor keeps the type's non-empty guarantee
    /// structural); any member's empty call id; a call id two members
    /// share (id-keyed pairing would be ambiguous).
    #[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
    pub(crate) fn try_new(calls: &[PendingCall]) -> Result<Self, RebuildError> {
        todo!()
    }

    /// The validated calls, in document order.
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[ValidatedCall] {
        &self.members
    }

    /// Pair every call with its outcome wire, positionally: the i-th
    /// outcome belongs to the i-th call in document order, so calls and
    /// outcomes cannot pair in the wrong order. One outcome per call; a
    /// count other than the calls' is a wiring fault in the segment
    /// drive ([`RebuildError::OutcomeCountMismatch`]). Consumes the
    /// validated calls: the unresolved list has no life after this.
    #[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
    pub(crate) fn resolve(
        self,
        outcomes: impl IntoIterator<Item = OutcomeWire>,
    ) -> Result<ResolvedCallBundle, RebuildError> {
        todo!()
    }
}

/// One bundle member: a validated pending call paired with the outcome
/// wire its tool result carries. The pair exists only as one type — a
/// call cannot travel without its outcome, and an outcome cannot be
/// attached to the wrong call by hand. Built only by
/// [`ValidatedCalls::resolve`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedCall {
    call: ValidatedCall,
    wire: OutcomeWire,
}

impl ResolvedCall {
    /// The paired call.
    #[must_use]
    pub(crate) fn call(&self) -> &ValidatedCall {
        &self.call
    }

    /// The outcome wire the rebuilt tool result carries.
    #[must_use]
    pub(crate) fn wire(&self) -> &OutcomeWire {
        &self.wire
    }
}

/// One awaiting node's resolved-call bundle: every pending call paired
/// with the outcome wire it must carry, in document order. Constructed
/// only by [`ValidatedCalls::resolve`], so [`rebuild_context`]'s input
/// is valid by construction — a mispaired or half-resolved bundle has no
/// type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedCallBundle {
    members: Vec<ResolvedCall>,
}

impl ResolvedCallBundle {
    /// The resolved calls, in document order.
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[ResolvedCall] {
        &self.members
    }
}

/// Why a node's bundle could not be built, or its context rebuilt.
/// Payloads are [`Diagnostic`]s — human text no caller branches on; the
/// preflight and the prelude render them into their fatal faults, and
/// the stage-2b frames pin the wordings.
#[derive(Debug, Clone)]
pub(crate) enum RebuildError {
    /// The node's call list parsed to no members. Unreachable in the
    /// wired flow — the seeding loop faults an awaiting node with an
    /// absent or empty pending list before the preflight runs — kept
    /// so the bundle types cannot be empty.
    EmptyCalls,
    /// A member carries an empty call id: the shape the gate's park arm
    /// records when the stream hook observed no tool-call id. The
    /// diagnostic names the member; no caller branches on it.
    EmptyCallId(Diagnostic),
    /// Two members share one call id: id-keyed pairing would be
    /// ambiguous. The diagnostic names the shared id; no caller branches
    /// on it.
    DuplicateCallId(Diagnostic),
    /// Resolution arrived with a count other than one outcome per call
    /// — a wiring fault in the segment drive, unreachable when the
    /// prelude invokes each call exactly once. The diagnostic names the
    /// counts; no caller branches on it.
    OutcomeCountMismatch(Diagnostic),
    /// The snapshot's current prompt is not the tool-result user message
    /// both park producers write — the one input no constructor has
    /// validated. The builder refuses rather than silently
    /// restructuring the turn boundary (the design record names this
    /// the builder's second failure mode).
    NotAToolResultPrompt,
}

impl fmt::Display for RebuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCalls => {
                f.write_str("the awaiting node's pending call list parsed to no calls")
            }
            Self::EmptyCallId(detail)
            | Self::DuplicateCallId(detail)
            | Self::OutcomeCountMismatch(detail) => write!(f, "{detail}"),
            Self::NotAToolResultPrompt => f.write_str(
                "the parked snapshot's current prompt is not the tool-result message \
                 the park producers write",
            ),
        }
    }
}

/// The rebuilt continuation context for one awaiting node: the history
/// and the prompt the continuation streams from. Constructed only by
/// [`rebuild_context`], which guarantees the pairing invariant — every
/// tool result in the combined context is preceded by the assistant
/// tool call keyed to the same id — and that the history the snapshot
/// captured is otherwise byte-preserved.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RebuiltContext {
    history: Vec<Message>,
    current_prompt: Message,
}

impl RebuiltContext {
    /// The rebuilt history: the snapshot's captured messages in order,
    /// with the synthesized assistant tool calls appended.
    #[must_use]
    pub(crate) fn history(&self) -> &[Message] {
        &self.history
    }

    /// The rebuilt prompt: the snapshot's prompt with every bundle
    /// call's tool result in place.
    #[must_use]
    pub(crate) fn current_prompt(&self) -> &Message {
        &self.current_prompt
    }

    /// Take the rebuilt context as the continuation's stream inputs,
    /// in the stream call's parameter order: `(prompt, history)`.
    #[must_use]
    pub(crate) fn into_parts(self) -> (Message, Vec<Message>) {
        (self.current_prompt, self.history)
    }
}

/// Rebuild one awaiting node's continuation context from its parked
/// snapshot and its resolved bundle.
///
/// For every bundle call in document order: the assistant tool call is
/// present in the rebuilt history — the snapshot's captured message
/// reused where the live park captured the call, a turn synthesized
/// from the call's own record (id, tool, arguments) where it did not —
/// and the rebuilt prompt carries exactly one tool result keyed to that
/// call's own id, carrying its outcome wire verbatim: the sentinel slot
/// replaced where one exists, the result appended where none does (a
/// later duplicate slot for the same id is collapsed — exactly one
/// result per call, never two). The history the snapshot captured is
/// otherwise byte-preserved, and tool results the prompt carries for
/// ids outside the bundle — a prior resume's outcomes riding a
/// re-parked snapshot — are preserved verbatim.
///
/// Output invariant: every tool result in the combined context is
/// preceded, somewhere ahead of it, by the assistant tool call keyed to
/// the same id; no sentinel slot for a bundle call survives.
///
/// The one failure mode the already-valid bundle cannot prevent: the
/// snapshot's current prompt must be the tool-result user message both
/// park producers write. Anything else — reachable only through a
/// malformed checkpoint document — is
/// [`RebuildError::NotAToolResultPrompt`]; the builder refuses rather
/// than silently restructuring the turn boundary.
#[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
pub(crate) fn rebuild_context(
    snapshot: &ParkSnapshot,
    bundle: &ResolvedCallBundle,
) -> Result<RebuiltContext, RebuildError> {
    todo!()
}
