//! HTTP client for POSTing approval decisions to the aura-web-server's
//! conversational ingress endpoint (`POST /v1/approvals/{decision_id}`).
//!
//! The CLI is the attended approval client: when the server parks a tool
//! call on the conversational route, it emits `aura.approval_pending` on
//! the SSE stream. The CLI renders the prompt, reads the human's decision,
//! and posts it back via [`ApprovalPoster`].
//!
//! Only constructed in HTTP mode — standalone mode has no HTTP ingress
//! endpoint to POST to.

use std::sync::Arc;

use reqwest::Client;
use serde::Serialize;

use crate::config::AppConfig;

/// The human's decision on an approval prompt.
///
/// `Approved` carries no reason — an approval reason is meaningless in
/// this protocol (the wire body is just `{"approved": true}`). `Denied`
/// carries an optional reason that the model sees as feedback, so it can
/// reason about why the human said no.
///
/// This is the CLI-side domain type; it converts to
/// [`ApprovalDecisionBody`] for the wire form, which mirrors
/// `aura::hitl::ApprovalDecisionWire` byte-identically without requiring
/// a dependency on the `aura` crate.
#[derive(Debug, Clone)]
pub enum ApprovalResponse {
    /// The human approved the tool call.
    Approved,
    /// The human denied the tool call, optionally with a reason.
    Denied {
        /// Why the human denied. `None` = "no reason provided".
        reason: Option<String>,
    },
}

/// The wire body for `POST /v1/approvals/{decision_id}`.
///
/// Semantically equivalent to `aura::hitl::ApprovalDecisionWire` — kept
/// here so the CLI doesn't pull in the `aura` crate for one struct. The
/// server deserializes with `ApprovalDecisionWire`, which uses
/// `#[serde(deny_unknown_fields)]`; this struct serializes the same
/// `approved` + optional `reason` shape.
#[derive(Debug, Serialize)]
struct ApprovalDecisionBody {
    approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl From<ApprovalResponse> for ApprovalDecisionBody {
    fn from(response: ApprovalResponse) -> Self {
        match response {
            ApprovalResponse::Approved => Self {
                approved: true,
                reason: None,
            },
            ApprovalResponse::Denied { reason } => Self {
                approved: false,
                reason,
            },
        }
    }
}

/// The result of posting a decision to the server.
///
/// Distinguishes "accepted" (204) from "not found" (404) so the caller
/// can render the right message. A 404 is a legitimate race — the
/// approval may have timed out while the human was deciding, or the
/// stream dropped and the server cancelled the park. Transport failures
/// and unexpected statuses are [`PostError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// 204 — the server accepted the decision; the parked call will resume.
    Accepted,
    /// 404 — no pending approval for this `decision_id`.
    ///
    /// The approval expired, was already resolved, or the stream
    /// disconnected and the server cancelled the park.
    NotFound,
}

/// Errors that can occur when posting an approval decision.
#[derive(Debug, thiserror::Error)]
pub enum PostError {
    /// Network/transport failure (DNS, connection refused, timeout).
    #[error("approval POST failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// Failed to read the error response body after an unexpected status.
    /// Distinct from [`PostError::Transport`](Self::Transport) (request-level)
    /// so callers can distinguish a body-read failure from a request failure.
    #[error("approval POST returned unexpected status and body-read failed: {source}")]
    BodyRead {
        status: u16,
        #[source]
        source: reqwest::Error,
    },
    /// Unexpected HTTP status (not 204 or 404).
    #[error("approval POST returned unexpected status {status}: {body}")]
    UnexpectedStatus { status: u16, body: String },
}

/// Client for POSTing approval decisions to the aura-web-server's
/// `/v1/approvals/{decision_id}` ingress endpoint.
///
/// Holds a reference to the CLI's [`AppConfig`] so auth headers and the
/// base URL are consistent with the chat completions client
/// ([`crate::api::client::ChatClient`]). The poster is only constructed
/// in HTTP mode — standalone mode has no HTTP ingress endpoint.
pub struct ApprovalPoster {
    http: Client,
    config: Arc<AppConfig>,
}

impl Clone for ApprovalPoster {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            config: Arc::clone(&self.config),
        }
    }
}

impl ApprovalPoster {
    #[must_use]
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            http: Client::new(),
            config,
        }
    }

    /// POST a decision to `/v1/approvals/{decision_id}`.
    ///
    /// Returns `Ok(PostOutcome)` for HTTP-level success (204 →
    /// [`PostOutcome::Accepted`], 404 → [`PostOutcome::NotFound`]).
    /// Returns `Err(PostError)` for transport failures or unexpected
    /// server errors.
    ///
    /// Auth headers (`Authorization: Bearer`, `extra_headers`) are
    /// applied identically to chat completion requests.
    pub async fn post_decision(
        &self,
        decision_id: &str,
        response: ApprovalResponse,
    ) -> Result<PostOutcome, PostError> {
        let url = self.config.approvals_url(decision_id);
        let body = ApprovalDecisionBody::from(response);

        let req = crate::api::client::apply_common_headers(
            &self.config,
            self.http.post(&url).json(&body),
            None,
        );

        let resp = req.send().await?;

        let status = resp.status().as_u16();
        match status {
            204 => Ok(PostOutcome::Accepted),
            404 => Ok(PostOutcome::NotFound),
            _ => match resp.text().await {
                Ok(text) => Err(PostError::UnexpectedStatus { status, body: text }),
                Err(source) => Err(PostError::BodyRead { status, source }),
            },
        }
    }
}
