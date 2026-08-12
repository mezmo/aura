//! Approver identity header overrides for gated MCP tool calls.
//!
//! When a HITL-gated tool call is approved over the webhook route, the
//! approval HTTP response may carry identity headers. This module holds the
//! types that carry those headers from capture (gate scope, webhook route
//! only) to application (exactly one outbound MCP request, via the rmcp
//! request-extension side-channel).

use aura_config::ToolHeaderMappings;
use reqwest::header::HeaderMap;

/// Validated approver identity headers captured from one approved webhook
/// response.
///
/// A value of this type exists only for an approved webhook decision whose
/// mapped response headers were all present and valid; a partial capture
/// is unrepresentable (construction fails closed). Construction is
/// crate-private: the webhook client's gate-scoped path is the only
/// producer.
#[derive(Clone)]
pub struct ApproverHeaders {
    /// The validated override pairs, keys lowercased so an override replaces
    /// the frozen default header rather than coexisting with it. The keys
    /// are also the audit surface (names only); no separate name list
    /// exists to fall out of sync.
    headers: HeaderMap,
}

impl ApproverHeaders {
    /// Capture and validate approver headers from an approval response.
    ///
    /// Every outbound name in `mapping` must resolve to a present response
    /// header or the whole capture fails closed; nothing is silently
    /// dropped.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled when capture wiring lands"
    )]
    #[expect(
        dead_code,
        reason = "filled when the webhook client's gate path wires capture"
    )]
    pub(crate) fn from_captured(
        mapping: &ToolHeaderMappings,
        response_headers: &HeaderMap,
    ) -> Result<Self, CaptureError> {
        todo!("resolve mapped response headers, fail closed on missing")
    }

    /// The captured outbound header names (never values), lowercased.
    pub fn captured_names(&self) -> impl Iterator<Item = &str> {
        self.headers.keys().map(reqwest::header::HeaderName::as_str)
    }

    /// Apply the overrides to an outbound request builder as per-request
    /// headers, which override the client's frozen `default_headers` for
    /// that one request only.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled when gate-to-wire wiring lands"
    )]
    pub(crate) fn apply_to(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        todo!("per-request .header(k, v) for each captured pair")
    }
}

impl std::fmt::Debug for ApproverHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApproverHeaders")
            .field("captured_names", &self.captured_names().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

// Manual impls rather than derives: `PreCallOutcome` derives `PartialEq, Eq`
// and must keep doing so; `Eq` is sound here because `HeaderValue` equality
// is total, and `HeaderMap` equality is order-insensitive.
impl PartialEq for ApproverHeaders {
    fn eq(&self, other: &Self) -> bool {
        self.headers == other.headers
    }
}

impl Eq for ApproverHeaders {}

/// Capture-time failures: the approved webhook response could not yield
/// the configured approver headers (fail closed).
///
/// The `names` payload is diagnostic-only text for the error message and
/// the event-level audit signal: always non-empty by construction (capture
/// fails only when at least one name is missing), lowercased outbound
/// header names, never values, and no domain logic branches on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// Mapped response headers absent from the approved response. Invalid
    /// values cannot occur here: capture reads a parsed `HeaderMap`, whose
    /// values are already syntactically valid; invalid outbound names are
    /// rejected earlier, at config parse.
    #[error("approver identity capture failed: response missing mapped headers {names:?}")]
    MissingHeaders { names: Vec<String> },
}

/// Application-time failures at the execution seam (double override,
/// transport refusal), kept separate from [`CaptureError`] so a
/// composition layer cannot wrap one as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OverrideApplicationError {
    /// More than one wrapper produced overrides for one call. Identity is
    /// never chosen by wrapper order; this is an error in release and debug
    /// alike.
    #[error("conflicting approver identity overrides from multiple wrappers")]
    DoubleOverride,
    /// The tool's transport cannot deliver per-call headers while identity
    /// was demanded (stdio fails closed).
    #[error("transport {kind:?} cannot deliver approver identity overrides")]
    TransportUnsupported { kind: McpTransportKind },
}

/// Which MCP transport an adaptor was constructed for. Tagged at
/// construction (the three `add_all_tools` branches) so the override path
/// can fail closed on transports that cannot deliver per-request headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransportKind {
    StreamableHttp,
    Sse,
    Stdio,
}

/// Fail closed when `kind` cannot deliver per-call header overrides.
/// Called at the execution seam only when overrides exist.
#[expect(
    unused_variables,
    reason = "todo!() body; filled when gate-to-wire wiring lands"
)]
pub(crate) fn ensure_transport_delivers_overrides(
    kind: McpTransportKind,
    overrides: &ApproverHeaders,
) -> Result<(), OverrideApplicationError> {
    todo!("stdio rejects overrides; http and sse accept")
}

/// Extract approver overrides from an outbound client message, if the one
/// request riding in it carries them as an extension. `None` for every
/// non-request message and every request without the extension. Shared by
/// the streamable-HTTP `post_message` and the SSE `Transport::send` read
/// points.
#[must_use]
pub(crate) fn extract_from_client_message(
    message: &rmcp::model::ClientJsonRpcMessage,
) -> Option<ApproverHeaders> {
    use rmcp::model::GetExtensions;
    match message {
        rmcp::model::JsonRpcMessage::Request(request) => request
            .request
            .extensions()
            .get::<ApproverHeaders>()
            .cloned(),
        _ => None,
    }
}

tokio::task_local! {
    /// Approver header overrides for the current gated tool call. Scoped
    /// by `WrappedTool::call` inside the inner-call spawn; read by
    /// `McpToolAdaptor::call` via [`current_approver_overrides`].
    /// Crate-private so nothing outside the wrapper path can inject
    /// overrides.
    pub(crate) static APPROVER_OVERRIDES: Option<ApproverHeaders>;
}

/// Unscoped-safe read of the current call's approver overrides.
///
/// `None` outside any scope. Unscoped reads are a permanent live path, not
/// an edge case: `McpToolAdaptor` is registered WITHOUT a `WrappedTool`
/// when no wrapper is configured. A `with`-based read would panic there;
/// this helper never does.
#[must_use]
pub(crate) fn current_approver_overrides() -> Option<ApproverHeaders> {
    APPROVER_OVERRIDES
        .try_with(std::clone::Clone::clone)
        .unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every adaptor call outside a task-local scope (wrapper-less agents)
    /// must read `None`, never panic.
    #[tokio::test]
    async fn unscoped_read_yields_none() {
        assert_eq!(current_approver_overrides(), None);
    }

    /// And the scoped read hands the value through unchanged shape-wise
    /// (a `None` payload scoped explicitly is still `None`).
    #[tokio::test]
    async fn scoped_none_reads_none() {
        APPROVER_OVERRIDES
            .scope(None, async {
                assert_eq!(current_approver_overrides(), None);
            })
            .await;
    }
}
