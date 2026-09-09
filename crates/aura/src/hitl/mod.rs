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
mod poller;
mod protocol;
mod registry;
mod route;
mod signing;
mod tool;

pub use decision::{
    AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, AwaitingDecision, CancelReason,
    DecisionId, ResolvedDecision, Timestamp,
};
pub(crate) use events::completed_cancelled;
/// Read one full HTTP/1.1 request (head plus content-length body) off a
/// test socket, returning the raw text. Shared by the scripted receivers in
/// the route and poller test modules.
#[cfg(test)]
pub(crate) async fn read_full_request(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until we have the complete header section (\r\n\r\n).
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    // Parse Content-Length so the request body is consumed before
    // responding — otherwise reqwest may see a connection reset while
    // still sending the POST body.
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let header_section = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length: usize = header_section
        .lines()
        .find(|line| line.to_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|val| val.trim().parse().ok())
        .unwrap_or(0);
    let body_already_read = buf.len() - header_end;
    let remaining = content_length.saturating_sub(body_already_read);
    if remaining > 0 {
        let mut body_buf = vec![0u8; remaining];
        socket.read_exact(&mut body_buf).await.unwrap();
        buf.extend_from_slice(&body_buf);
    }
    String::from_utf8_lossy(&buf).to_string()
}

pub use gate::HitlApprovalWrapper;
// Re-exported so config-fingerprint tests construct the poll client the
// production way.
pub use poller::{PollReconciler, PollerHandle};
pub use protocol::{ApprovalDecisionWire, ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
pub use registry::{ParkedApproval, PendingApprovals, ResolveError};
#[cfg(test)]
pub(crate) use route::webhook_client_from_config;
pub use route::{
    ApprovalError, DecisionRoute, HitlRuntime, PlaintextWebhookUrlError, WebhookClient,
    cleartext_capture_warning, validate_webhook_signing_config, warn_on_cleartext_capture,
};
pub use signing::{
    ConfigError, PrimarySecret, SIGNATURE_HEADER, SignedHeaders, SigningContext, TIMESTAMP_HEADER,
    Tolerance, VerificationError, VerifiedBody, WebhookHmac, authorize_ingress,
};
pub use tool::{RequestApprovalArgs, RequestApprovalTool};
