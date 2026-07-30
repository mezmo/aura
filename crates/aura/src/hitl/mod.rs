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
//! Both routes are fully operational in both single-agent and orchestration mode:
//!
//! - **Webhook** (Route A): [`HitlApprovalWrapper`], [`RequestApprovalTool`],
//!   [`DecisionRoute::Webhook`], domain types, wire protocol, and SSE events.
//! - **Conversational** (Route B): [`DecisionRoute::Conversational`] parks on
//!   the registry; decisions arrive via `POST /v1/approvals/{decision_id}`
//!   (web-server) or in-process `PendingApprovals::resolve()` (CLI standalone).
//!
//! Single-agent mode composes the gate and tool in [`Agent::new`](crate::builder::Agent::new);
//! orchestration workers compose them per-task in `create_worker`.

mod decision;
mod events;
mod gate;
mod protocol;
mod registry;
mod route;
mod signing;
mod teardown;
mod tool;

pub use decision::{
    AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, AwaitingDecision, CancelReason,
    DecisionId, Timestamp,
};
pub use gate::{BlockedSignal, HitlApprovalWrapper};
pub use protocol::{ApprovalDecisionWire, ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
pub use registry::{ParkedApproval, PendingApprovals, ResolveError};
pub use route::{
    ApprovalError, DecisionRoute, HitlRuntime, PlaintextWebhookUrlError, WebhookClient,
    validate_webhook_signing_config,
};
pub use signing::{
    SIGNATURE_HEADER, SignedHeaders, SigningContext, TIMESTAMP_HEADER, VerificationError,
    VerifiedBody, WebhookHmac, authorize_ingress,
};
pub use teardown::{ApprovalOwnership, TeardownUnderway};
pub use tool::{RequestApprovalArgs, RequestApprovalTool};
