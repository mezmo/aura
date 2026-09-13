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
//! This stage-2b unit fills the five behavior bodies (the frames in the
//! test module below pin them); P45 stage 3 wired the substitution prelude
//! to this module (the stage 7 reap then deleted the retired slot-swap
//! helper from `continuation`, with its unit frame), and the design
//! record — including the repair-round panel ledger and the stage-2b
//! coverage manifest — is `REBUILD-DESIGN.md` beside this module.

use std::collections::{HashMap, HashSet};
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
    fn try_new(calls: &[PendingCall]) -> Result<Self, PreflightError> {
        if calls.is_empty() {
            return Err(PreflightError::EmptyCalls(Diagnostic::new(
                "the pending call list parsed to no calls",
            )));
        }
        if let Some((member, _)) = calls
            .iter()
            .enumerate()
            .find(|(_, call)| call.call_id.is_empty())
        {
            return Err(PreflightError::EmptyCallId(Diagnostic::new(format!(
                "pending call member {member} carries an empty call id"
            ))));
        }
        if let Some((member, _)) = calls
            .iter()
            .enumerate()
            .find(|(_, call)| call.tool_name.is_empty())
        {
            return Err(PreflightError::EmptyToolName(Diagnostic::new(format!(
                "pending call member {member} carries an empty tool name"
            ))));
        }
        for (member, call) in calls.iter().enumerate() {
            if calls[..member]
                .iter()
                .any(|earlier| earlier.call_id == call.call_id)
            {
                return Err(PreflightError::DuplicateCallId(Diagnostic::new(format!(
                    "two pending calls share the call id {}",
                    call.call_id
                ))));
            }
        }
        Ok(Self {
            members: calls
                .iter()
                .map(|call| ValidatedCall {
                    decision_id: call.decision_id,
                    tool_name: call.tool_name.clone(),
                    arguments: call.arguments.clone(),
                    call_id: CallId(call.call_id.clone()),
                })
                .collect(),
        })
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
    pub(crate) fn resolve(
        self,
        outcomes: impl IntoIterator<Item = (CallId, OutcomeWire)>,
    ) -> Result<ResolvedCallBundle, ResolveError> {
        let mut wires: HashMap<CallId, OutcomeWire> = HashMap::new();
        for (id, wire) in outcomes {
            if !self.members.iter().any(|call| call.call_id == id) {
                return Err(ResolveError::UnknownOutcomeId(Diagnostic::new(format!(
                    "an outcome is keyed to the call id {}, which this node's \
                     bundle does not carry",
                    id.as_ref()
                ))));
            }
            if wires.contains_key(&id) {
                return Err(ResolveError::DuplicateOutcomeId(Diagnostic::new(format!(
                    "two outcomes are keyed to the call id {}",
                    id.as_ref()
                ))));
            }
            wires.insert(id, wire);
        }
        let members = self
            .members
            .into_iter()
            .map(|call| {
                let wire = wires.remove(&call.call_id).ok_or_else(|| {
                    ResolveError::MissingOutcome(Diagnostic::new(format!(
                        "the pending call {} (tool {}) has no outcome keyed to it",
                        call.call_id.as_ref(),
                        call.tool_name
                    )))
                })?;
                Ok(ResolvedCall { call, wire })
            })
            .collect::<Result<Vec<_>, ResolveError>>()?;
        Ok(ResolvedCallBundle { members })
    }
}

/// The tool-result-prompt witness: a snapshot prompt validated to be
/// the user message that carries tool results — a `User` message with
/// at least one tool-result item — the only shape both park producers
/// write, and the only one [`rebuild_context`] can carry outcomes in.
/// Constructed fallibly at PREFLIGHT, where the shape is knowable
/// before any tombstone or invocation; the builder requires the
/// witness, so it is total.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolResultPrompt(Message);

impl ToolResultPrompt {
    /// Validate a snapshot prompt as the tool-result user message: a
    /// `User` message carrying at least one tool result. Both
    /// refusals — an assistant prompt, and a user prompt with no tool
    /// result at all — are shapes no producer writes: the live park
    /// always stamps the gate-tripping call's sentinel, so a
    /// tool-result-less `current_prompt` is a checkpoint no producer
    /// produces. The refusal,
    /// [`PreflightError::NotAToolResultPrompt`], is reachable only
    /// through a malformed checkpoint document, and fires before any
    /// tombstone or invocation — never inside the builder, and never
    /// as a silent restructuring of the turn boundary.
    pub(crate) fn try_new(prompt: &Message) -> Result<Self, PreflightError> {
        match prompt {
            Message::User { content }
                if content
                    .iter()
                    .any(|item| matches!(item, rig::message::UserContent::ToolResult(_))) =>
            {
                Ok(Self(prompt.clone()))
            }
            Message::User { .. } | Message::Assistant { .. } => {
                Err(PreflightError::NotAToolResultPrompt(Diagnostic::new(
                    "the parked snapshot's current prompt is not the tool-result message \
                     the park producers write",
                )))
            }
        }
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
    pub(crate) fn try_new(nodes: &[NodePreflightInput<'_>]) -> Result<Self, PreflightError> {
        let mut validated = Vec::with_capacity(nodes.len());
        for (node, input) in nodes.iter().enumerate() {
            let calls =
                ValidatedCalls::try_new(input.calls).map_err(|fault| fault.named_to_node(node))?;
            let prompt = ToolResultPrompt::try_new(input.prompt)
                .map_err(|fault| fault.named_to_node(node))?;
            validated.push(ValidatedNode { calls, prompt });
        }
        Ok(Self { nodes: validated })
    }

    /// Take the halves for the drive, in input order.
    #[must_use]
    pub(crate) fn into_nodes(self) -> Vec<ValidatedNode> {
        self.nodes
    }
}

/// Why the segment preflight refused. Every variant carries one
/// [`Diagnostic`] — human text no caller branches on; the preflight's
/// consumer renders them into its fatal fault, and the stage-2b frames
/// pin the wordings.
#[derive(Debug, Clone)]
pub(crate) enum PreflightError {
    /// A node's call list parsed to no members. The diagnostic is
    /// named to its node at the segment door; no caller branches on the
    /// text. Unreachable in the wired flow — the seeding loop faults an
    /// awaiting node with an absent or empty pending list before the
    /// preflight runs — kept so the bundle types cannot be empty.
    EmptyCalls(Diagnostic),
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
    /// both park producers write: an assistant prompt, or a user prompt
    /// carrying no tool result at all — the live park always stamps the
    /// gate-tripping call's sentinel, so a tool-result-less
    /// `current_prompt` is a checkpoint no producer writes. The
    /// diagnostic is named to its node at the segment door; no caller
    /// branches on the text. Reachable only through a malformed
    /// checkpoint document; refused at the preflight, before any
    /// tombstone or invocation, never restructured inside the builder.
    NotAToolResultPrompt(Diagnostic),
}

impl fmt::Display for PreflightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCalls(detail)
            | Self::EmptyCallId(detail)
            | Self::DuplicateCallId(detail)
            | Self::EmptyToolName(detail)
            | Self::NotAToolResultPrompt(detail) => write!(f, "{detail}"),
        }
    }
}

impl PreflightError {
    /// Name a per-node fault to its node: the segment door validates
    /// every awaiting node together, so a diagnostic a caller renders
    /// must say which node refused. Private: only the door re-diagnoses.
    fn named_to_node(self, node: usize) -> Self {
        let named = |detail: Diagnostic| Diagnostic::new(format!("awaiting node {node}: {detail}"));
        match self {
            Self::EmptyCalls(detail) => Self::EmptyCalls(named(detail)),
            Self::EmptyCallId(detail) => Self::EmptyCallId(named(detail)),
            Self::DuplicateCallId(detail) => Self::DuplicateCallId(named(detail)),
            Self::EmptyToolName(detail) => Self::EmptyToolName(named(detail)),
            Self::NotAToolResultPrompt(detail) => Self::NotAToolResultPrompt(named(detail)),
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
    /// with the synthesized assistant turn appended. Frame-only: the
    /// production path takes the context apart through `into_parts`.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn history(&self) -> &[Message] {
        &self.history
    }

    /// The rebuilt prompt: the snapshot's prompt with every bundle
    /// call's tool result in place. Frame-only: the production path
    /// takes the context apart through `into_parts`.
    #[allow(dead_code)]
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
pub(crate) fn rebuild_context(
    history: &[Message],
    prompt: ToolResultPrompt,
    bundle: &ResolvedCallBundle,
) -> RebuiltContext {
    // The history half: every bundle call the snapshot captured is reused
    // where it stands; every call it missed lands in ONE appended assistant
    // turn — the producer's same-completion shape — synthesized from the
    // call's own record.
    let mut synthesized = Vec::new();
    for resolved in bundle.as_slice() {
        let call = resolved.call();
        let captured = history.iter().any(|message| {
            let Message::Assistant { content, .. } = message else {
                return false;
            };
            content.iter().any(|item| {
                matches!(
                    item,
                    rig::message::AssistantContent::ToolCall(tool_call)
                        if tool_call.id == call.call_id.as_ref()
                )
            })
        });
        if !captured {
            let ValidatedCall {
                call_id: CallId(id),
                tool_name,
                arguments,
                ..
            } = call;
            synthesized.push(rig::message::AssistantContent::ToolCall(
                rig::message::ToolCall {
                    id: id.clone(),
                    call_id: None,
                    function: rig::message::ToolFunction {
                        name: tool_name.clone(),
                        arguments: arguments.clone(),
                    },
                    signature: None,
                    additional_params: None,
                },
            ));
        }
    }
    let mut rebuilt_history = history.to_vec();
    if !synthesized.is_empty() {
        rebuilt_history.push(Message::Assistant {
            id: None,
            content: rig::OneOrMany::many(synthesized)
                .expect("the synthesized turn carries at least one tool call"),
        });
    }

    // The prompt half: each bundle call's first slot replaced with its
    // outcome wire, later duplicate slots for the same id collapsed, and
    // the result appended where no slot exists — everything else rides
    // verbatim, extras included (the trust boundary).
    let ToolResultPrompt(prompt) = prompt;
    let Message::User { content } = prompt else {
        unreachable!("the preflight witness validated the prompt as the user message");
    };
    let mut replaced: HashSet<&CallId> = HashSet::new();
    let mut items: Vec<rig::message::UserContent> = Vec::new();
    for item in content.into_iter() {
        match item {
            rig::message::UserContent::ToolResult(mut slot) => {
                let Some(resolved) = bundle
                    .as_slice()
                    .iter()
                    .find(|resolved| resolved.call().call_id().as_ref() == slot.id)
                else {
                    items.push(rig::message::UserContent::ToolResult(slot));
                    continue;
                };
                if replaced.insert(resolved.call().call_id()) {
                    slot.content = rig::OneOrMany::one(rig::message::ToolResultContent::text(
                        resolved.wire().as_ref(),
                    ));
                    items.push(rig::message::UserContent::ToolResult(slot));
                }
            }
            extra => items.push(extra),
        }
    }
    for resolved in bundle.as_slice() {
        if !replaced.contains(resolved.call().call_id()) {
            let ValidatedCall {
                call_id: CallId(id),
                ..
            } = resolved.call();
            items.push(rig::message::UserContent::ToolResult(
                rig::message::ToolResult {
                    id: id.clone(),
                    call_id: None,
                    content: rig::OneOrMany::one(rig::message::ToolResultContent::text(
                        resolved.wire().as_ref(),
                    )),
                },
            ));
        }
    }
    RebuiltContext {
        history: rebuilt_history,
        current_prompt: Message::User {
            content: rig::OneOrMany::many(items)
                .expect("the rebuilt prompt keeps at least one content item"),
        },
    }
}

#[cfg(test)]
mod tests {
    //! The P45 stage-2b frames over the five behavior bodies. Builder
    //! frames assert the complete rebuilt `(history, current_prompt)`
    //! sequence as exact rig messages — whole-context pins, not substring
    //! probes; the preflight and resolution frames pin each refusal variant
    //! together with its diagnostic wording; the serialization frame
    //! grounds provider-bound acceptance at the wire form the repo's own
    //! serializer calibration uses (the Bedrock-seam exclusion, its
    //! reason, and its owner are recorded in REBUILD-DESIGN.md's coverage
    //! section).

    use std::collections::HashSet;

    use super::*;
    use serde_json::json;

    /// The park sentinel, mirrored from the gate's private wording (the
    /// goldens mirror it too): the placeholder text a decided resume must
    /// remove from the rebuilt context.
    const PARK_SENTINEL: &str =
        "This tool call is parked pending human approval. It has not run. Do not retry.";

    /// The frames' call ids, tools, and decision ids, fixed so every
    /// expected message and pinned diagnostic is literal.
    const CALL_A: &str = "call_apply_1";
    const CALL_B: &str = "call_scale_2";
    const CALL_C: &str = "call_delete_3";
    const CALL_PRIOR: &str = "call_get_0";
    const CALL_EXTRA: &str = "call_get_extra";
    const TOOL_A: &str = "kubectl_apply";
    const TOOL_B: &str = "kubectl_scale";
    const TOOL_C: &str = "kubectl_delete";
    const TOOL_PRIOR: &str = "kubectl_get";
    const DECISION_A: &str = "0199c0de-4545-7000-8000-000000000042";
    const DECISION_B: &str = "0199c0de-4545-7000-8000-000000000046";
    const DECISION_C: &str = "0199c0de-4545-7000-8000-000000000047";
    /// The outcome wires in the chain's JSON-quoted rendering (rig
    /// JSON-serializes tool outputs), as resolution receives them.
    const WIRE_A: &str = "\"applied to prod\"";
    const WIRE_B: &str = "\"scaled to 3 replicas\"";
    const WIRE_C: &str = "\"deleted the stale deployment\"";
    const WIRE_PRIOR: &str = "\"listed three deployments\"";
    const WIRE_EXTRA: &str = "\"listed two services\"";

    fn decision(id: &str) -> DecisionId {
        DecisionId::parse(id).expect("the fixed frame decision id parses")
    }

    fn args_a() -> Value {
        json!({ "namespace": "prod" })
    }

    fn args_b() -> Value {
        json!({ "namespace": "prod", "replicas": 3 })
    }

    fn args_c() -> Value {
        json!({ "namespace": "prod", "label": "stale" })
    }

    /// One pending call under the given identity.
    fn pending(decision_id: DecisionId, tool: &str, args: Value, call_id: &str) -> PendingCall {
        PendingCall {
            decision_id,
            tool_name: tool.to_string(),
            arguments: args,
            call_id: call_id.to_string(),
        }
    }

    /// The standard first call of the frames' fixtures.
    fn pending_a() -> PendingCall {
        pending(decision(DECISION_A), TOOL_A, args_a(), CALL_A)
    }

    /// The pivot fixture's second call: a distinct tool, arguments, and id.
    fn pending_b() -> PendingCall {
        pending(decision(DECISION_B), TOOL_B, args_b(), CALL_B)
    }

    /// The synthesis-grouping fixture's third call.
    fn pending_c() -> PendingCall {
        pending(decision(DECISION_C), TOOL_C, args_c(), CALL_C)
    }

    /// The sentinel tool-result slot keyed to one pending call id: the
    /// checkpoint placeholder shape a live park writes, carrying the
    /// sentinel in its JSON-quoted wire form.
    fn sentinel_slot(call_id: &str) -> rig::message::UserContent {
        rig::message::UserContent::ToolResult(rig::message::ToolResult {
            id: call_id.to_string(),
            call_id: None,
            content: rig::OneOrMany::one(rig::message::ToolResultContent::text(
                serde_json::to_string(PARK_SENTINEL).expect("the sentinel text serializes"),
            )),
        })
    }

    /// The tool result the rebuilt prompt carries for one call: the
    /// outcome in its wire form, keyed to the call's own id.
    fn result_slot(call_id: &str, wire: &str) -> rig::message::UserContent {
        rig::message::UserContent::ToolResult(rig::message::ToolResult {
            id: call_id.to_string(),
            call_id: None,
            content: rig::OneOrMany::one(rig::message::ToolResultContent::text(wire)),
        })
    }

    /// The user message carrying the given tool-result slots, in order.
    fn tool_result_prompt(slots: Vec<rig::message::UserContent>) -> Message {
        Message::User {
            content: rig::OneOrMany::many(slots).expect("the prompt carries a tool result"),
        }
    }

    /// The assistant tool call a checkpointed history carries for one gated
    /// call: keyed by the pending call id, carrying the recorded tool and
    /// arguments.
    fn assistant_tool_call(
        call_id: &str,
        tool: &str,
        args: &Value,
    ) -> rig::message::AssistantContent {
        rig::message::AssistantContent::ToolCall(rig::message::ToolCall {
            id: call_id.to_string(),
            call_id: None,
            function: rig::message::ToolFunction {
                name: tool.to_string(),
                arguments: args.clone(),
            },
            signature: None,
            additional_params: None,
        })
    }

    /// The assistant turn carrying the given tool calls: the history shape
    /// a live park captures — and the shape one-turn synthesis must
    /// reproduce for the calls it appends.
    fn tool_call_turn(calls: Vec<rig::message::AssistantContent>) -> Message {
        Message::Assistant {
            id: None,
            content: rig::OneOrMany::many(calls).expect("the turn carries a tool call"),
        }
    }

    /// A user text item — non-tool-result content a prompt may carry
    /// alongside its tool results.
    fn text_item(text: &str) -> rig::message::UserContent {
        rig::message::UserContent::Text(rig::message::Text {
            text: text.to_string(),
        })
    }

    /// The preflight's refusal for the given nodes — the door's `Err` half,
    /// taken without relying on a `Debug` impl the door does not carry.
    fn preflight_refusal(nodes: &[NodePreflightInput<'_>]) -> PreflightError {
        SegmentPreflight::try_new(nodes)
            .err()
            .expect("the frame's fixture refuses the preflight")
    }

    /// Stage one node's fixture through the preflight door and take its
    /// validated call list — the position the wired prelude resolves from.
    fn validated_calls(pending: &[PendingCall], prompt: &Message) -> ValidatedCalls {
        let preflight = SegmentPreflight::try_new(&[NodePreflightInput::new(pending, prompt)])
            .expect("the frame's fixture passes the segment preflight");
        preflight
            .into_nodes()
            .pop()
            .expect("the one-node fixture validates one node")
            .into_parts()
            .0
    }

    /// Drive one node's fixture through both construction phases: the
    /// segment preflight (one node — the door, never the private
    /// constructor), then keyed resolution over the given `(call id, wire)`
    /// pairs — the caller obligation the wired prelude meets: exactly one
    /// outcome per bundle call, keyed by the validated calls' own ids.
    /// Pair order is arbitrary: resolution pairs by identity.
    fn resolved_bundle(
        pending: &[PendingCall],
        prompt: &Message,
        outcomes: &[(&str, &str)],
    ) -> (ToolResultPrompt, ResolvedCallBundle) {
        let preflight = SegmentPreflight::try_new(&[NodePreflightInput::new(pending, prompt)])
            .expect("the frame's fixture passes the segment preflight");
        let node = preflight
            .into_nodes()
            .pop()
            .expect("the one-node fixture validates one node");
        let (calls, witness) = node.into_parts();
        // The caller obligation: take the ids off the validated list BEFORE
        // resolution consumes it, keying one outcome to each call's own id.
        let pairs: Vec<(CallId, OutcomeWire)> = calls
            .as_slice()
            .iter()
            .map(|call| {
                let (_, wire) = outcomes
                    .iter()
                    .find(|(id, _)| *id == call.call_id().as_ref())
                    .expect("the frame wires one outcome per call");
                (call.call_id().clone(), OutcomeWire::new(*wire))
            })
            .collect();
        let bundle = calls.resolve(pairs).expect("the wired outcomes resolve");
        (witness, bundle)
    }

    /// Rebuild a frame's context end to end from its snapshot pieces.
    fn rebuilt(
        history: &[Message],
        prompt: &Message,
        pending: &[PendingCall],
        outcomes: &[(&str, &str)],
    ) -> RebuiltContext {
        let (witness, bundle) = resolved_bundle(pending, prompt, outcomes);
        rebuild_context(history, witness, &bundle)
    }

    /// The pivot frame's snapshot pieces: a history capturing the first
    /// call's turn only, a prompt slotting the first call only, and the
    /// fixture's two pending calls — the live park's shape when the
    /// re-driven worker pivoted to a second gated call the snapshot missed.
    fn pivot_fixture() -> (Vec<Message>, Message, Vec<PendingCall>) {
        (
            vec![
                Message::user("apply it"),
                tool_call_turn(vec![assistant_tool_call(CALL_A, TOOL_A, &args_a())]),
            ],
            tool_result_prompt(vec![sentinel_slot(CALL_A)]),
            vec![pending_a(), pending_b()],
        )
    }

    /// FRAME 1 (pivot shape): the park missed the second call's tool call,
    /// so the rebuild appends exactly ONE assistant turn — carrying that
    /// call synthesized from its own record — replaces the first call's
    /// sentinel slot in place, appends the second call's result in document
    /// order, keeps every captured message byte-preserved, and leaves the
    /// sentinel nowhere.
    #[test]
    fn pivot_shape_appends_one_synthesized_turn_and_carries_both_results_in_document_order() {
        let (history, prompt, pending) = pivot_fixture();
        let context = rebuilt(
            &history,
            &prompt,
            &pending,
            &[(CALL_A, WIRE_A), (CALL_B, WIRE_B)],
        );

        let expected_history = vec![
            Message::user("apply it"),
            tool_call_turn(vec![assistant_tool_call(CALL_A, TOOL_A, &args_a())]),
            tool_call_turn(vec![assistant_tool_call(CALL_B, TOOL_B, &args_b())]),
        ];
        assert_eq!(
            context.history(),
            expected_history.as_slice(),
            "the rebuilt history is the captured pair plus exactly one \
             synthesized assistant turn carrying the missed call"
        );
        assert_eq!(
            &context.history()[..history.len()],
            history.as_slice(),
            "the captured messages ride the rebuilt history byte-preserved"
        );
        let expected_prompt = tool_result_prompt(vec![
            result_slot(CALL_A, WIRE_A),
            result_slot(CALL_B, WIRE_B),
        ]);
        assert_eq!(
            context.current_prompt(),
            &expected_prompt,
            "the first call's slot is replaced in place and the second call's \
             result is appended, in document order"
        );
        let serialized = serde_json::to_string(&(context.history(), context.current_prompt()))
            .expect("the rebuilt context serializes");
        assert!(
            !serialized.contains(PARK_SENTINEL),
            "the sentinel survives nowhere in the rebuilt context"
        );
    }

    /// FRAME 2 (two-slot shape): the snapshot captured both calls in one
    /// assistant turn and slotted both sentinels — the rebuild replaces
    /// each slot in place and touches no history message.
    #[test]
    fn two_slot_shape_replaces_both_slots_in_place_and_keeps_the_history_byte_identical() {
        let history = vec![
            Message::user("apply it"),
            tool_call_turn(vec![
                assistant_tool_call(CALL_A, TOOL_A, &args_a()),
                assistant_tool_call(CALL_B, TOOL_B, &args_b()),
            ]),
        ];
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A), sentinel_slot(CALL_B)]);
        let context = rebuilt(
            &history,
            &prompt,
            &[pending_a(), pending_b()],
            &[(CALL_A, WIRE_A), (CALL_B, WIRE_B)],
        );

        assert_eq!(
            context.history(),
            history.as_slice(),
            "the captured history is byte-identical: reused calls never move, \
             and nothing is synthesized"
        );
        let expected_prompt = tool_result_prompt(vec![
            result_slot(CALL_A, WIRE_A),
            result_slot(CALL_B, WIRE_B),
        ]);
        assert_eq!(
            context.current_prompt(),
            &expected_prompt,
            "both sentinel slots are replaced in place, in the snapshot's order"
        );
    }

    /// FRAME 3 (pairing invariant, over both shapes): in the combined
    /// rebuilt context — history, then prompt — every tool result is
    /// preceded by an assistant tool call keyed to the same id, each id
    /// issued once and answered exactly once.
    #[test]
    fn every_rebuilt_tool_result_is_preceded_by_its_own_call_and_answers_once() {
        let (pivot_history, pivot_prompt, pivot_pending) = pivot_fixture();
        let pivot = rebuilt(
            &pivot_history,
            &pivot_prompt,
            &pivot_pending,
            &[(CALL_A, WIRE_A), (CALL_B, WIRE_B)],
        );

        let two_slot_history = vec![
            Message::user("apply it"),
            tool_call_turn(vec![
                assistant_tool_call(CALL_A, TOOL_A, &args_a()),
                assistant_tool_call(CALL_B, TOOL_B, &args_b()),
            ]),
        ];
        let two_slot = rebuilt(
            &two_slot_history,
            &tool_result_prompt(vec![sentinel_slot(CALL_A), sentinel_slot(CALL_B)]),
            &[pending_a(), pending_b()],
            &[(CALL_A, WIRE_A), (CALL_B, WIRE_B)],
        );

        assert_pairing_invariant(&pivot);
        assert_pairing_invariant(&two_slot);
    }

    /// Walk the combined rebuilt context — history, then the prompt — and
    /// hold the provider pairing rule: every tool result id was issued by
    /// an EARLIER assistant tool call, each id issued once, each id
    /// answered exactly once.
    fn assert_pairing_invariant(context: &RebuiltContext) {
        let mut issued: HashSet<&str> = HashSet::new();
        let mut answered: HashSet<&str> = HashSet::new();
        for message in context
            .history()
            .iter()
            .chain(std::iter::once(context.current_prompt()))
        {
            match message {
                Message::Assistant { content, .. } => {
                    for item in content.iter() {
                        if let rig::message::AssistantContent::ToolCall(tool_call) = item {
                            assert!(
                                issued.insert(tool_call.id.as_str()),
                                "the assistant call {} appears exactly once",
                                tool_call.id
                            );
                        }
                    }
                }
                Message::User { content } => {
                    for item in content.iter() {
                        if let rig::message::UserContent::ToolResult(tool_result) = item {
                            assert!(
                                issued.contains(tool_result.id.as_str()),
                                "the tool result {} is preceded by its own assistant \
                                 tool call",
                                tool_result.id
                            );
                            assert!(
                                answered.insert(tool_result.id.as_str()),
                                "the tool result {} appears exactly once",
                                tool_result.id
                            );
                        }
                    }
                }
            }
        }
    }

    /// FRAME 4 (synthesis grouping): three pending calls with only the
    /// first captured — BOTH missed calls land in ONE appended assistant
    /// turn, in document order.
    #[test]
    fn three_call_pivot_groups_both_missing_calls_into_one_appended_turn_in_document_order() {
        let (history, prompt, _) = pivot_fixture();
        let context = rebuilt(
            &history,
            &prompt,
            &[pending_a(), pending_b(), pending_c()],
            &[(CALL_A, WIRE_A), (CALL_B, WIRE_B), (CALL_C, WIRE_C)],
        );

        let expected_history = vec![
            Message::user("apply it"),
            tool_call_turn(vec![assistant_tool_call(CALL_A, TOOL_A, &args_a())]),
            tool_call_turn(vec![
                assistant_tool_call(CALL_B, TOOL_B, &args_b()),
                assistant_tool_call(CALL_C, TOOL_C, &args_c()),
            ]),
        ];
        assert_eq!(
            context.history(),
            expected_history.as_slice(),
            "the second and third calls ride ONE synthesized assistant turn, \
             in document order"
        );
        let expected_prompt = tool_result_prompt(vec![
            result_slot(CALL_A, WIRE_A),
            result_slot(CALL_B, WIRE_B),
            result_slot(CALL_C, WIRE_C),
        ]);
        assert_eq!(
            context.current_prompt(),
            &expected_prompt,
            "the captured slot is replaced and both missing results are \
             appended, in document order"
        );
    }

    /// FRAME 5 (preserve extras): tool results for ids OUTSIDE the bundle
    /// inherit their producer's pairing. ONE rebuild carries BOTH extra
    /// kinds — a prior resume's paired call and result in the history, a
    /// non-bundle result in the prompt — and the WHOLE rebuilt context
    /// (history and prompt together) is asserted in one pass, so an
    /// interaction that reshapes the history only when prompt extras
    /// exist cannot hide behind a split assertion.
    #[test]
    fn extras_outside_the_bundle_ride_verbatim_in_history_and_prompt() {
        let history = vec![
            Message::user("deploy it"),
            tool_call_turn(vec![assistant_tool_call(CALL_PRIOR, TOOL_PRIOR, &args_a())]),
            tool_result_prompt(vec![result_slot(CALL_PRIOR, WIRE_PRIOR)]),
            tool_call_turn(vec![assistant_tool_call(CALL_A, TOOL_A, &args_a())]),
        ];
        let prompt = tool_result_prompt(vec![
            result_slot(CALL_EXTRA, WIRE_EXTRA),
            sentinel_slot(CALL_A),
        ]);
        let context = rebuilt(&history, &prompt, &[pending_a()], &[(CALL_A, WIRE_A)]);

        let expected_prompt = tool_result_prompt(vec![
            result_slot(CALL_EXTRA, WIRE_EXTRA),
            result_slot(CALL_A, WIRE_A),
        ]);
        assert_eq!(
            (context.history(), context.current_prompt()),
            (history.as_slice(), &expected_prompt),
            "the whole rebuilt context over one rebuild: the prior resume's \
             paired call and result ride the history byte-preserved (nothing is \
             synthesized), and the non-bundle prompt result keeps the snapshot's \
             position and bytes while the bundle slot is replaced in place"
        );
    }

    /// FRAME (Gate A, id-only replacement): a slot for a bundle id whose
    /// content is NOT the sentinel — a stale real result an earlier turn
    /// recorded — is replaced BY CALL ID: the old text is gone, the
    /// call's own outcome wire rides the slot. An implementation keying
    /// on sentinel text instead of id fails here.
    #[test]
    fn a_non_sentinel_slot_for_a_bundle_id_is_replaced_by_id_and_its_old_text_gone() {
        let history = vec![
            Message::user("apply it"),
            tool_call_turn(vec![assistant_tool_call(CALL_A, TOOL_A, &args_a())]),
        ];
        let stale = "\"a stale result an earlier turn recorded\"";
        let prompt = tool_result_prompt(vec![result_slot(CALL_A, stale)]);
        let context = rebuilt(&history, &prompt, &[pending_a()], &[(CALL_A, WIRE_A)]);

        assert_eq!(
            context.history(),
            history.as_slice(),
            "the captured call is reused; nothing is synthesized"
        );
        let expected_prompt = tool_result_prompt(vec![result_slot(CALL_A, WIRE_A)]);
        assert_eq!(
            context.current_prompt(),
            &expected_prompt,
            "replacement keys on the call id, not the sentinel text: the stale \
             result is gone and the call's own outcome rides the slot"
        );
        let serialized =
            serde_json::to_string(context.current_prompt()).expect("the rebuilt prompt serializes");
        assert!(
            !serialized.contains(stale),
            "the replaced slot's old text survives nowhere"
        );
    }

    /// FRAME (Gate A, duplicate-slot collapse): two slots for ONE bundle
    /// id — a malformed document the producers do not write, held to the
    /// collapse rule — the FIRST slot is replaced with the call's wire
    /// where it sat, the later duplicate is collapsed out, and exactly
    /// one result for the id survives.
    #[test]
    fn duplicate_slots_for_one_bundle_id_collapse_to_one_replaced_result_at_the_first_slot() {
        let history = vec![
            Message::user("apply it"),
            tool_call_turn(vec![
                assistant_tool_call(CALL_A, TOOL_A, &args_a()),
                assistant_tool_call(CALL_B, TOOL_B, &args_b()),
            ]),
        ];
        let prompt = tool_result_prompt(vec![
            sentinel_slot(CALL_A),
            sentinel_slot(CALL_B),
            sentinel_slot(CALL_A),
        ]);
        let context = rebuilt(
            &history,
            &prompt,
            &[pending_a(), pending_b()],
            &[(CALL_A, WIRE_A), (CALL_B, WIRE_B)],
        );

        assert_eq!(
            context.history(),
            history.as_slice(),
            "the duplicate-slot shape synthesizes nothing: both calls are captured"
        );
        let expected_prompt = tool_result_prompt(vec![
            result_slot(CALL_A, WIRE_A),
            result_slot(CALL_B, WIRE_B),
        ]);
        assert_eq!(
            context.current_prompt(),
            &expected_prompt,
            "the first slot for the id is replaced where it sat, the later \
             duplicate is collapsed, and exactly one result per id survives"
        );
        assert_pairing_invariant(&context);
    }

    /// FRAME 6a: an empty call id — the gate's park arm's stamp when the
    /// stream hook observed no id — refuses the preflight with the named
    /// variant and diagnostic.
    #[test]
    fn empty_call_id_refuses_the_segment_preflight() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A)]);
        let refusal = preflight_refusal(&[NodePreflightInput::new(
            &[pending(decision(DECISION_A), TOOL_A, args_a(), "")],
            &prompt,
        )]);
        assert!(
            matches!(refusal, PreflightError::EmptyCallId(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "awaiting node 0: pending call member 0 carries an empty call id",
            "the diagnostic names the node and the member"
        );
    }

    /// FRAME 6b: an empty tool name — no registered tool carries one —
    /// refuses the preflight.
    #[test]
    fn empty_tool_name_refuses_the_segment_preflight() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A)]);
        let refusal = preflight_refusal(&[NodePreflightInput::new(
            &[pending(decision(DECISION_A), "", args_a(), CALL_A)],
            &prompt,
        )]);
        assert!(
            matches!(refusal, PreflightError::EmptyToolName(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "awaiting node 0: pending call member 0 carries an empty tool name",
            "the diagnostic names the node and the member"
        );
    }

    /// FRAME 6c: two members sharing one call id refuse the preflight —
    /// id-keyed pairing would be ambiguous.
    #[test]
    fn duplicate_call_ids_refuse_the_segment_preflight() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A)]);
        let refusal = preflight_refusal(&[NodePreflightInput::new(
            &[
                pending(decision(DECISION_A), TOOL_A, args_a(), CALL_A),
                pending(decision(DECISION_B), TOOL_B, args_b(), CALL_A),
            ],
            &prompt,
        )]);
        assert!(
            matches!(refusal, PreflightError::DuplicateCallId(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "awaiting node 0: two pending calls share the call id call_apply_1",
            "the diagnostic names the shared id"
        );
    }

    /// FRAME 6d: an empty pending list refuses — unreachable in the wired
    /// flow (the seeding loop faults it earlier), kept so the bundle types
    /// cannot be empty. The refusal is named to its node like every other
    /// preflight fault.
    #[test]
    fn empty_pending_list_refuses_the_segment_preflight() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A)]);
        let refusal = preflight_refusal(&[NodePreflightInput::new(&[], &prompt)]);
        assert!(
            matches!(refusal, PreflightError::EmptyCalls(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "awaiting node 0: the pending call list parsed to no calls",
            "the refusal is named to its node"
        );
    }

    /// FRAME 6e: a snapshot prompt that is not the tool-result user message
    /// — an assistant message, reachable only through a malformed document
    /// — refuses the preflight before any tombstone or invocation, named
    /// to its node like every other preflight fault.
    #[test]
    fn assistant_prompt_refuses_the_segment_preflight() {
        let assistant_prompt = Message::Assistant {
            id: None,
            content: rig::OneOrMany::one(rig::message::AssistantContent::Text(
                rig::message::Text {
                    text: "model prose".to_string(),
                },
            )),
        };
        let refusal =
            preflight_refusal(&[NodePreflightInput::new(&[pending_a()], &assistant_prompt)]);
        assert!(
            matches!(refusal, PreflightError::NotAToolResultPrompt(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "awaiting node 0: the parked snapshot's current prompt is not the \
             tool-result message the park producers write",
            "the refusal is named to its node"
        );
    }

    /// FRAME (Gate A, witness tightening): a User prompt carrying NO tool
    /// result at all — the pre-A1 bare-text shape, a checkpoint no
    /// producer writes (the live park always stamps the gate-tripping
    /// call's sentinel) — refuses the preflight, named to its node.
    #[test]
    fn text_only_user_prompt_refuses_the_segment_preflight() {
        let refusal = preflight_refusal(&[NodePreflightInput::new(
            &[pending_a()],
            &Message::user("tool results"),
        )]);
        assert!(
            matches!(refusal, PreflightError::NotAToolResultPrompt(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "awaiting node 0: the parked snapshot's current prompt is not the \
             tool-result message the park producers write",
            "a text-only user message is not the tool-result prompt any \
             producer writes"
        );
    }

    /// FRAME (Gate A, witness tightening): a User prompt mixing text and a
    /// tool result passes the preflight — both producers can aggregate
    /// content alongside results — and the builder preserves the text
    /// item verbatim, replacing only the bundle call's slot.
    #[test]
    fn mixed_text_and_tool_result_prompt_passes_and_the_text_rides_verbatim() {
        let history = vec![
            Message::user("apply it"),
            tool_call_turn(vec![assistant_tool_call(CALL_A, TOOL_A, &args_a())]),
        ];
        let prompt = tool_result_prompt(vec![
            text_item("here is what ran so far"),
            sentinel_slot(CALL_A),
        ]);
        let context = rebuilt(&history, &prompt, &[pending_a()], &[(CALL_A, WIRE_A)]);

        assert_eq!(
            context.history(),
            history.as_slice(),
            "the captured call is reused; nothing is synthesized"
        );
        let expected_prompt = tool_result_prompt(vec![
            text_item("here is what ran so far"),
            result_slot(CALL_A, WIRE_A),
        ]);
        assert_eq!(
            context.current_prompt(),
            &expected_prompt,
            "the text item rides verbatim in its position; the bundle slot is \
             replaced in place"
        );
    }

    /// FRAME 7 (segment door): node A alone would validate; node B's empty
    /// call id refuses the WHOLE segment, the fault named to node B — the
    /// all-or-nothing property.
    #[test]
    fn one_nodes_invalid_input_refuses_the_whole_segment_at_the_door() {
        let a_pending = [pending_a()];
        let a_prompt = tool_result_prompt(vec![sentinel_slot(CALL_A)]);
        SegmentPreflight::try_new(&[NodePreflightInput::new(&a_pending, &a_prompt)])
            .expect("node A alone validates");

        let b_pending = [pending(decision(DECISION_B), TOOL_B, args_b(), "")];
        let b_prompt = tool_result_prompt(vec![sentinel_slot(CALL_B)]);
        let refusal = preflight_refusal(&[
            NodePreflightInput::new(&a_pending, &a_prompt),
            NodePreflightInput::new(&b_pending, &b_prompt),
        ]);
        assert!(
            matches!(refusal, PreflightError::EmptyCallId(_)),
            "the refusal is node B's own fault: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "awaiting node 1: pending call member 0 carries an empty call id",
            "the fault is named to node B, and surfaces though node A validated"
        );
    }

    /// FRAME 8a: outcomes arriving OUT OF ORDER still pair by identity —
    /// the bundle keeps document order, each call carrying the outcome
    /// keyed to its own id.
    #[test]
    fn out_of_order_outcomes_still_pair_by_id() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A), sentinel_slot(CALL_B)]);
        let calls = validated_calls(&[pending_a(), pending_b()], &prompt);
        let bundle = calls
            .resolve([
                (CallId(CALL_B.to_string()), OutcomeWire::new(WIRE_B)),
                (CallId(CALL_A.to_string()), OutcomeWire::new(WIRE_A)),
            ])
            .expect("identity, not position, pairs the outcomes");

        let paired: Vec<(&str, &str)> = bundle
            .as_slice()
            .iter()
            .map(|resolved| (resolved.call().call_id().as_ref(), resolved.wire().as_ref()))
            .collect();
        assert_eq!(
            paired,
            [(CALL_A, WIRE_A), (CALL_B, WIRE_B)],
            "the bundle keeps document order; each call carries the outcome \
             keyed to its own id"
        );
    }

    /// FRAME 8b: an outcome keyed to an id the bundle does not carry is
    /// refused — a wiring bug's guard, pinned with its wording.
    #[test]
    fn outcome_keyed_outside_the_bundle_refuses() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A)]);
        let calls = validated_calls(&[pending_a()], &prompt);
        let refusal = calls
            .resolve([(CallId("call_ghost".to_string()), OutcomeWire::new(WIRE_A))])
            .expect_err("the unknown outcome id refuses resolution");
        assert!(
            matches!(refusal, ResolveError::UnknownOutcomeId(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "an outcome is keyed to the call id call_ghost, which this node's \
             bundle does not carry",
            "the diagnostic names the unknown id"
        );
    }

    /// FRAME 8c: two outcomes keyed to one id are refused.
    #[test]
    fn duplicate_outcome_keys_refuse() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A)]);
        let calls = validated_calls(&[pending_a()], &prompt);
        let refusal = calls
            .resolve([
                (CallId(CALL_A.to_string()), OutcomeWire::new(WIRE_A)),
                (CallId(CALL_A.to_string()), OutcomeWire::new(WIRE_B)),
            ])
            .expect_err("the duplicate outcome key refuses resolution");
        assert!(
            matches!(refusal, ResolveError::DuplicateOutcomeId(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "two outcomes are keyed to the call id call_apply_1",
            "the diagnostic names the duplicated id"
        );
    }

    /// FRAME 8d: a call left without its outcome is refused.
    #[test]
    fn call_left_without_its_outcome_refuses() {
        let prompt = tool_result_prompt(vec![sentinel_slot(CALL_A), sentinel_slot(CALL_B)]);
        let calls = validated_calls(&[pending_a(), pending_b()], &prompt);
        let refusal = calls
            .resolve([(CallId(CALL_A.to_string()), OutcomeWire::new(WIRE_A))])
            .expect_err("the unpaired call refuses resolution");
        assert!(
            matches!(refusal, ResolveError::MissingOutcome(_)),
            "the refusal variant: {refusal:?}"
        );
        assert_eq!(
            refusal.to_string(),
            "the pending call call_scale_2 (tool kubectl_scale) has no outcome \
             keyed to it",
            "the diagnostic names the abandoned call"
        );
    }

    /// FRAME 9 (provider acceptance): the rebuilt context survives the rig
    /// Message wire round-trip — every rebuilt message serializes to the
    /// JSON wire form and parses back identical, sentinel-free. The
    /// Bedrock request-encode itself is NOT reachable from this crate —
    /// the exclusion row in REBUILD-DESIGN.md records the seam, the
    /// reason, and its owner; this round-trip is the same serializer the
    /// resume goldens calibrate their wire literals against.
    #[test]
    fn rebuilt_context_survives_the_rig_message_serialization_round_trip() {
        let (history, prompt, pending) = pivot_fixture();
        let context = rebuilt(
            &history,
            &prompt,
            &pending,
            &[(CALL_A, WIRE_A), (CALL_B, WIRE_B)],
        );

        for message in context
            .history()
            .iter()
            .chain(std::iter::once(context.current_prompt()))
        {
            let wire = serde_json::to_value(message).expect("the rebuilt message serializes");
            let back: Message =
                serde_json::from_value(wire).expect("the wire form parses back into a message");
            assert_eq!(&back, message, "the wire round-trips the rebuilt message");
        }
        let serialized = serde_json::to_string(&(context.history(), context.current_prompt()))
            .expect("the rebuilt context serializes");
        assert!(
            !serialized.contains(PARK_SENTINEL),
            "no sentinel rides the provider-bound wire"
        );
    }
}
