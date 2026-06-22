//! Webhook wire protocol: the payload sent to an approval webhook (Route A) and
//! the shape the conversational ingress mirrors.

use aura_events::{AgentScopeWire, ApprovalOriginWire};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::decision::{AgentScope, ApprovalDecision, ApprovalOrigin, DecisionId};

/// Current approval webhook protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// The payload describing one approval request.
// No `Serialize`: the request goes on the wire only through `ApprovalRequestWire`
// (flat, rename-stable). Keeping the domain struct unserializable makes the
// variant-name leak unrepresentable rather than convention-guarded.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// Protocol version; always [`PROTOCOL_VERSION`].
    pub version: u32,
    /// The handle a decision resolves against.
    pub decision_id: DecisionId,
    /// The global request id (SSE routing + MCP cancellation), modeled as the
    /// existing bare `String` id used throughout the codebase.
    ///
    /// A `RequestId` newtype is deliberately not introduced. Unlike RunId /
    /// SessionId / TaskIdentity it has no single owning module, and it threads
    /// through SSE routing, the tool event broker, and MCP cancellation, so
    /// branding it is a cross-cutting refactor out of scope here. The design
    /// note's `RequestId` typing is aspirational.
    pub request_id: String,
    /// Who is asking.
    pub scope: AgentScope,
    /// Why this approval exists.
    pub origin: ApprovalOrigin,
    /// The tool call(s) awaiting approval.
    pub items: Vec<ApprovalItem>,
}

/// A single tool call awaiting approval.
///
/// Per the design note the spike's per-item `matched_pattern` and `task` are
/// gone: that information lives on [`ApprovalRequest::origin`] and
/// [`ApprovalRequest::scope`] respectively.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalItem {
    pub tool_name: String,
    pub arguments: Value,
}

/// Wire form of a single decision: the `{ "approved": bool, "reason": ... }`
/// body shared by the webhook response (Route A) and the decision ingress
/// (Route B, `POST /v1/approvals/{decision_id}`). The boundary that turns the
/// wire body into the domain [`ApprovalDecision`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecisionWire {
    pub approved: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

impl From<ApprovalDecisionWire> for ApprovalDecision {
    fn from(wire: ApprovalDecisionWire) -> Self {
        if wire.approved {
            ApprovalDecision::Approved
        } else {
            ApprovalDecision::Denied {
                reason: wire.reason,
            }
        }
    }
}

/// The webhook request projected to its wire form: the flat, rename-stable JSON
/// AURA POSTs to an approval webhook (Route A).
///
/// Only `scope` and `origin` are converted, from the externally-tagged domain
/// enums (whose JSON would otherwise carry Rust variant names) to the flat
/// `aura_events` wire enums. `version`, `decision_id` (`#[serde(transparent)]`
/// over `Uuid`, so already a string), `request_id`, and `items` already
/// serialize stably, so they borrow or pass through and the (potentially large)
/// tool `arguments` are never cloned.
///
/// Built by `impl From<&ApprovalRequest>` in [`super::events`]; `Serialize`-only,
/// since the reply is the separate [`ApprovalDecisionWire`] and no client ever
/// deserializes this. Not an `aura-events` stream event, so it lives here rather
/// than in that crate.
#[derive(Debug, Serialize)]
pub(crate) struct ApprovalRequestWire<'a> {
    pub version: u32,
    pub decision_id: DecisionId,
    pub request_id: &'a str,
    pub scope: AgentScopeWire,
    pub origin: ApprovalOriginWire,
    pub items: &'a [ApprovalItem],
}
