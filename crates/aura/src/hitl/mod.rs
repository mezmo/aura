//! Human-in-the-loop approval gating for agent tool calls.
//!
//! Two surfaces gate tool execution behind a human (or webhook) decision:
//!
//! - the **config gate** ([`HitlApprovalWrapper`]) intercepts tool calls whose
//!   name matches a configured glob, transparently to the agent; and
//! - the **agent-requested** surface ([`RequestApprovalTool`]) is a tool the
//!   agent calls when it judges that an action needs a human.
//!
//! Both resolve through a [`DecisionRoute`] fixed per deployment by the
//! `[hitl.route]` config table: a synchronous webhook (unattended) or an
//! in-process park answered by `POST /v1/approvals/{decision_id}` (attended).
//! The lifecycle is fail-closed: only [`ApprovalOutcome::Decided`] with
//! [`ApprovalDecision::Approved`] runs the gated call.
//!
//! See `docs/design/hitl.md` and `docs/adr/2026-06-16-hitl-approval-architecture.md`.
//!
//! ## Implementation status
//!
//! The webhook route (Phase 1) is implemented: [`HitlApprovalWrapper`],
//! [`RequestApprovalTool`], [`DecisionRoute::Webhook`], the domain types in
//! `decision.rs`, the wire protocol in `protocol.rs`, and the SSE event
//! conversion in `events.rs` are all working.
//!
//! The conversational route (Phase 2) is wired: the
//! [`DecisionRoute::Conversational`] arm parks on the registry and awaits a
//! decision via `POST /v1/approvals/{decision_id}`.

mod decision;
mod events;
mod gate;
mod protocol;
mod registry;
mod route;
mod tool;

pub use decision::{
    AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, AwaitingDecision, CancelReason,
    DecisionId, Timestamp,
};
pub use gate::HitlApprovalWrapper;
pub use protocol::{ApprovalDecisionWire, ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
pub use registry::{ParkedApproval, PendingApprovals, ResolveError};
pub use route::{ApprovalError, DecisionRoute, HitlRuntime, WebhookClient};
pub use tool::{RequestApprovalArgs, RequestApprovalTool};
