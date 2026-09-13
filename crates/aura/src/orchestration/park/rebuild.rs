//! The provider-valid context builder for the reconstruction direction
//! (P45, R5): one awaiting node's parked conversation rebuilt so the
//! continuation streams a context in which every BUNDLE call's tool
//! result is preceded by the assistant tool call of the same id — the
//! pairing providers require; an orphaned tool result is a protocol
//! violation everywhere the chain speaks. Tool results the snapshot
//! already carries for ids OUTSIDE the bundle inherit the producer's
//! pairing and are preserved verbatim — the trust boundary (see the
//! design record).
//!
//! Construction runs in two phases, because the outcome wires exist
//! only after the segment invokes the decided calls:
//!
//! 1. **Preflight (fallible, segment-wide).**
//!    [`SegmentPreflight::try_new`] takes EVERY awaiting node's
//!    pending-call list and snapshot prompt and validates them together:
//!    ordered, non-empty, every call id non-empty and unique within its
//!    node, every tool name non-empty, and every prompt the
//!    tool-result user message both park producers write
//!    ([`ToolResultPrompt::try_new`]). No node's validated half exists
//!    unless every node validated — the preflight runs before any
//!    tombstone or invocation, so one bad call id (the gate's park arm
//!    can stamp an empty one when the stream hook observed no
//!    tool-call id), one empty tool name, or one malformed prompt
//!    refuses the whole segment before a tool runs.
//! 2. **Resolution (keyed).** [`ValidatedCalls::resolve`] pairs each
//!    call with the outcome wire keyed to the call's OWN id — pairing
//!    by identity, not position, so calls and outcomes cannot pair in
//!    the wrong order. An outcome keyed to an id the bundle does not
//!    carry, a key used twice, and a call left without its outcome are
//!    all refused.
//!
//! [`rebuild_context`] then rebuilds the node's `(history,
//! current_prompt)` — TOTAL, both inputs validated by construction. For
//! every bundle call in document order: the assistant tool call is
//! present in the history — reused where the snapshot captured it,
//! synthesized from the call's own record where the park missed it
//! (every missing call of the node lands in ONE assistant turn, the
//! producer's same-completion shape) — and the prompt carries exactly
//! one tool result keyed to the call's own id, carrying its outcome
//! wire verbatim: the sentinel slot replaced where one exists, the
//! result appended where none does. The captured history is otherwise
//! byte-preserved — a prior resume's outcomes riding a re-parked
//! snapshot live there; prompt-carried extras, the stage 4 producer's
//! shape, are preserved verbatim.
//!
//! This stage-2 unit lays the type surface only: the five behavior
//! bodies are `todo!()` (filled by P45 stage 2b), the module is unwired
//! (P45 stage 3 points the substitution prelude at it; the plan's
//! stage 7 reaps `continuation::replace_tool_result` last), and the
//! design record — including the repair-round panel ledger — is
//! `REBUILD-DESIGN.md` beside this module.

#![allow(dead_code)] // unwired: P45 stage 3 wires the builder into the
// substitution prelude; sweep this allow together with the re-export's
// unused-imports marker in park/mod.rs when the wiring lands

use std::fmt;

use rig::completion::Message;
use serde_json::Value;

use super::resume::Diagnostic;
use crate::hitl::DecisionId;
use crate::orchestration::PendingCall;

/// A pending call's non-empty tool-call id: the key every rebuilt tool
/// result rides under, the id the reconstructed assistant tool call
/// carries, and — after the keyed repair — the identity an outcome wire
/// pairs by. Constructed only inside this module, after the non-empty
/// check: an empty id (the shape the gate's park arm records when the
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
/// with the live denial text. Total: every rendered text is a valid
/// wire form. This is the only constructor on the type, so no bare
/// `String` reaches resolution without passing it; the carried-verbatim
/// rule itself (never re-render, never double-encode) travels by
/// convention and audit, not by this type.
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
/// [`ValidatedCalls::try_new`]; everything but the id and the tool name
/// is the checkpoint's record carried verbatim.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValidatedCall {
    decision_id: DecisionId,
    tool_name: String,
    arguments: Value,
    call_id: CallId,
}

impl ValidatedCall {
    /// The call's validated tool-call id — the reconstruction's key and
    /// the identity resolution pairs by.
    #[must_use]
    pub(crate) fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The gated tool's recorded name, validated non-empty.
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
/// non-empty, no id shared by two calls, and every tool name non-empty.
/// A transition state: [`ValidatedCalls::resolve`] consumes it, and it
/// is constructible only through [`SegmentPreflight::try_new`] — no
/// bundle exists for any node unless every node of the segment
/// validated.
#[derive(Debug, PartialEq)]
pub(crate) struct ValidatedCalls {
    members: Vec<ValidatedCall>,
}

impl ValidatedCalls {
    /// Parse the node's pending calls. Private: the segment preflight is
    /// the only door. Refusals, in parse order: an empty list (a node
    /// with no pending calls faults earlier, in seeding — this keeps
    /// the type's non-empty guarantee structural); any member's empty
    /// call id; any member's empty tool name; a call id two members
    /// share (id-keyed pairing would be ambiguous).
    #[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
    fn try_new(calls: &[PendingCall]) -> Result<Self, PreflightError> {
        todo!()
    }

    /// The validated calls, in document order.
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[ValidatedCall] {
        &self.members
    }

    /// Pair each call with the outcome wire keyed to its OWN id —
    /// pairing by identity, not position: an outcome can reach only the
    /// call whose id it carries. Refused: an outcome keyed to an id the
    /// bundle does not carry ([`ResolveError::UnknownOutcomeId`]); a key
    /// used twice ([`ResolveError::DuplicateOutcomeId`]); a call left
    /// with no outcome ([`ResolveError::MissingOutcome`]). The
    /// surviving caller obligation: exactly one outcome per bundle
    /// call, keyed by the validated calls' own ids — taken from
    /// [`ValidatedCall::call_id`] before this consumes the list. The
    /// unresolved list has no life after this.
    #[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
    pub(crate) fn resolve(
        self,
        outcomes: impl IntoIterator<Item = (CallId, OutcomeWire)>,
    ) -> Result<ResolvedCallBundle, ResolveError> {
        todo!()
    }
}

/// The tool-result-prompt witness: a snapshot prompt validated to be
/// the user message that carries tool results — the only shape both
/// park producers write, and the only one [`rebuild_context`] can
/// carry outcomes in. Constructed fallibly at PREFLIGHT, where the
/// shape is knowable before any tombstone or invocation; the builder
/// requires the witness, so it is total.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolResultPrompt(Message);

impl ToolResultPrompt {
    /// Validate a snapshot prompt as the tool-result user message. The
    /// refusal, [`PreflightError::NotAToolResultPrompt`], is reachable
    /// only through a malformed checkpoint document, and fires before
    /// any tombstone or invocation — never inside the builder, and
    /// never as a silent restructuring of the turn boundary.
    #[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
    pub(crate) fn try_new(prompt: &Message) -> Result<Self, PreflightError> {
        todo!()
    }
}

impl AsRef<Message> for ToolResultPrompt {
    fn as_ref(&self) -> &Message {
        &self.0
    }
}

/// One awaiting node's preflight input: its pending-call list and its
/// snapshot's prompt — the two facts one node contributes to the
/// segment check, paired as one type so a prompt cannot be attributed
/// to another node's calls.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NodePreflightInput<'a> {
    calls: &'a [PendingCall],
    prompt: &'a Message,
}

impl<'a> NodePreflightInput<'a> {
    /// Stage one node's preflight input.
    #[must_use]
    pub(crate) fn new(calls: &'a [PendingCall], prompt: &'a Message) -> Self {
        Self { calls, prompt }
    }
}

/// One awaiting node's validated preflight half: its parsed calls and
/// its tool-result-prompt witness. Built only inside
/// [`SegmentPreflight::try_new`], and only when EVERY node of the
/// segment validated.
#[derive(Debug, PartialEq)]
pub(crate) struct ValidatedNode {
    calls: ValidatedCalls,
    prompt: ToolResultPrompt,
}

impl ValidatedNode {
    /// The node's validated calls, in document order.
    #[must_use]
    pub(crate) fn calls(&self) -> &ValidatedCalls {
        &self.calls
    }

    /// The node's validated prompt witness.
    #[must_use]
    pub(crate) fn prompt(&self) -> &ToolResultPrompt {
        &self.prompt
    }

    /// Take the half apart for the drive: the calls to resolve, the
    /// witness the builder consumes.
    #[must_use]
    pub(crate) fn into_parts(self) -> (ValidatedCalls, ToolResultPrompt) {
        (self.calls, self.prompt)
    }
}

/// The segment-level door: EVERY awaiting node's preflight validated
/// together, or nothing. The fallible constructor takes all nodes'
/// inputs and yields the per-node validated halves only when the last
/// node validated too — no bundle exists for any node unless every node
/// validated, literally by construction: [`ValidatedCalls::try_new`]
/// is private to this module, and this constructor is its only caller.
/// Run it before any tombstone or invocation, so one node's invalid
/// input refuses the segment before another node's tool runs.
pub(crate) struct SegmentPreflight {
    nodes: Vec<ValidatedNode>,
}

impl SegmentPreflight {
    /// Validate every awaiting node's input: per node, the call checks
    /// of [`ValidatedCalls::try_new`] plus the prompt check of
    /// [`ToolResultPrompt::try_new`]. The first fault — named to its
    /// node by the diagnostic — refuses the whole segment; no half is
    /// retained.
    #[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
    pub(crate) fn try_new(nodes: &[NodePreflightInput<'_>]) -> Result<Self, PreflightError> {
        todo!()
    }

    /// The validated halves, in input order.
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[ValidatedNode] {
        &self.nodes
    }

    /// Take the halves for the drive, in input order.
    #[must_use]
    pub(crate) fn into_nodes(self) -> Vec<ValidatedNode> {
        self.nodes
    }
}

/// Why the segment preflight refused. Payloads are [`Diagnostic`]s —
/// human text no caller branches on; the preflight's consumer renders
/// them into its fatal fault, and the stage-2b frames pin the wordings.
#[derive(Debug, Clone)]
pub(crate) enum PreflightError {
    /// A node's call list parsed to no members. Unreachable in the
    /// wired flow — the seeding loop faults an awaiting node with an
    /// absent or empty pending list before the preflight runs — kept so
    /// the bundle types cannot be empty.
    EmptyCalls,
    /// A member carries an empty call id: the shape the gate's park arm
    /// records when the stream hook observed no tool-call id. The
    /// diagnostic names the member; no caller branches on it.
    EmptyCallId(Diagnostic),
    /// Two members of one node share a call id: id-keyed pairing would
    /// be ambiguous. The diagnostic names the shared id; no caller
    /// branches on it.
    DuplicateCallId(Diagnostic),
    /// A member carries an empty tool name — no registered tool has
    /// one, so a document stamping it is malformed; refusing at the
    /// preflight closes the synthesis surface by construction. The
    /// diagnostic names the member; no caller branches on it.
    EmptyToolName(Diagnostic),
    /// A node's snapshot prompt is not the tool-result user message
    /// both park producers write — reachable only through a malformed
    /// checkpoint document. Refused at the preflight, before any
    /// tombstone or invocation, never restructured inside the builder.
    NotAToolResultPrompt,
}

impl fmt::Display for PreflightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCalls => {
                f.write_str("the awaiting node's pending call list parsed to no calls")
            }
            Self::EmptyCallId(detail)
            | Self::DuplicateCallId(detail)
            | Self::EmptyToolName(detail) => write!(f, "{detail}"),
            Self::NotAToolResultPrompt => f.write_str(
                "the parked snapshot's current prompt is not the tool-result message \
                 the park producers write",
            ),
        }
    }
}

/// One bundle member: a validated pending call paired with the outcome
/// wire its tool result carries. The pair exists only as one type, and
/// only by identity — resolution pairs the wire keyed to the call's own
/// id — so a call cannot travel without its outcome, and an outcome
/// cannot be attached to the wrong call by hand. Built only by
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
/// is valid by construction — a mispaired or half-resolved bundle has
/// no type.
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

/// Why resolution could not pair the outcomes. Payloads are
/// [`Diagnostic`]s — diagnostic-only, like the preflight's. The keyed
/// pairing decomposes the earlier count-mismatch fault into its two
/// halves: an outcome keyed outside the bundle is
/// [`ResolveError::UnknownOutcomeId`], and a call left without its
/// outcome is [`ResolveError::MissingOutcome`]; the same key arriving
/// twice is [`ResolveError::DuplicateOutcomeId`]. All three are wiring
/// guards — unreachable when the prelude invokes each call exactly
/// once and keys its outcomes by the validated calls' own ids.
#[derive(Debug, Clone)]
pub(crate) enum ResolveError {
    /// An outcome keyed to an id the bundle does not carry. The
    /// diagnostic names the id; no caller branches on it.
    UnknownOutcomeId(Diagnostic),
    /// Two outcomes keyed to one id. The diagnostic names the id; no
    /// caller branches on it.
    DuplicateOutcomeId(Diagnostic),
    /// A bundle call no outcome was keyed to. The diagnostic names the
    /// call; no caller branches on it.
    MissingOutcome(Diagnostic),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOutcomeId(detail)
            | Self::DuplicateOutcomeId(detail)
            | Self::MissingOutcome(detail) => write!(f, "{detail}"),
        }
    }
}

/// The rebuilt continuation context for one awaiting node: the history
/// and the prompt the continuation streams from. Constructed only by
/// [`rebuild_context`].
///
/// Invariant, scoped to BUNDLE-KEYED results: every bundle call's tool
/// result in the combined context is preceded by the assistant tool
/// call keyed to the same id, and the history the snapshot captured is
/// otherwise byte-preserved — extras included. Tool results the
/// snapshot already carries for ids OUTSIDE the bundle inherit the
/// PRODUCER's pairing and are preserved verbatim: the trust boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RebuiltContext {
    history: Vec<Message>,
    current_prompt: Message,
}

impl RebuiltContext {
    /// The rebuilt history: the snapshot's captured messages in order,
    /// with the synthesized assistant turn appended.
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

/// Rebuild one awaiting node's continuation context — TOTAL: both
/// inputs are validated by construction, the prompt by
/// [`ToolResultPrompt::try_new`] at the preflight, the bundle by
/// [`ValidatedCalls::resolve`] at resolution. Pure: nothing outside
/// its three inputs is consulted.
///
/// For every bundle call in document order: the assistant tool call is
/// present in the rebuilt history — the snapshot's captured message
/// reused where the live park captured the call, a turn synthesized
/// from the call's own record (id, tool, arguments) where it did not —
/// and every missing call of the node lands in ONE assistant turn, the
/// producer's same-completion shape. The rebuilt prompt carries exactly
/// one tool result keyed to that call's own id, carrying its outcome
/// wire verbatim: the sentinel slot replaced where one exists, the
/// result appended where none does (a later duplicate slot for the same
/// id is collapsed — exactly one result per call, never two). The
/// history the snapshot captured is otherwise byte-preserved; tool
/// results the prompt carries for ids outside the bundle are preserved
/// verbatim, their pairing the producer's (the trust boundary).
///
/// Output invariant, bundle-keyed only: no sentinel slot for a bundle
/// call survives.
#[expect(unused_variables, reason = "todo!() body; filled by P45 stage 2b")]
pub(crate) fn rebuild_context(
    history: &[Message],
    prompt: ToolResultPrompt,
    bundle: &ResolvedCallBundle,
) -> RebuiltContext {
    todo!()
}
